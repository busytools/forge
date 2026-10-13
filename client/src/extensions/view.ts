/**
 * What the Extensions page draws, derived from one read.
 *
 * Every derivation is here rather than in the components, so the page's
 * rules - which rows count toward what, when a plugin is stale, what the
 * band says - are testable without rendering anything.
 */

import type {
  ExtensionsSnapshot,
  ExtensionsRead,
  LspCheck,
  PluginComponents,
  RunRow,
  RunRowStatus,
  UpdateRun,
} from '../wire/extensions';

/** The rail's destinations, in order. `marketplaces` rides below a hairline. */
export type PaneId =
  'installed' | 'skills' | 'agents' | 'commands' | 'hooks' | 'lsp' | 'marketplaces';

/** The component kinds a fold or a pane can show. */
export type ComponentKind = 'skills' | 'agents' | 'commands' | 'hooks' | 'lsp';

/** One row inside an open plugin's fold, or in a kind's own pane. */
export interface ComponentRow {
  /** The component's own name (a hook row's is its plugin). */
  name: string;
  /** What rides the row: hook trigger events, or an LSP server's PATH check. */
  note: string | null;
}

/** One group inside a fold: a kind, its count, and every row of it. */
export interface ComponentGroup {
  kind: ComponentKind;
  label: string;
  count: number;
  rows: ComponentRow[];
}

/**
 * The mark a row carries, or `null` for the quiet case.
 *
 * Three shapes in this page's whole vocabulary (Ved, 2026-10-10): a
 * triangle for an update, a spinner for work in flight, an X for a
 * failure. Everything else is a chip in words - `auto-installed`,
 * `restart required`, `disabled` - because a vocabulary of seven shapes
 * is decode-work, not information.
 */
export type RowMark = 'update' | 'running' | 'failed' | null;

/** One plugin as the Installed pane draws it. */
export interface PluginRow {
  /** The full id `name@marketplace`, which is what a command names. */
  id: string;
  name: string;
  marketplace: string;
  version: string | null;
  /** Present only when the marketplace has a different version. */
  availableVersion: string | null;
  /** The counts line: `15 skills · 6 agents · hooks`. */
  detail: string;
  /**
   * The registry scope the install sits in (`user`, `project`, `local`).
   * What an Update names: the entry's own scope is sent, and a
   * hardcoded `user` would make every other scope unfixable from here.
   */
  scope: string;
  auto: boolean;
  enabled: boolean;
  /** Why the row could not load, when the scan failed to read it. */
  loadError: string | null;
  /** The fold's groups, in a fixed order; empty when the plugin ships none. */
  groups: ComponentGroup[];
  /** This plugin's row in the live update run, when it has one. */
  runStatus: RunRowStatus | null;
  /** That run row's own words: a failure's reason, or the restart contract
   * the CLI stated (`restart required to apply`). */
  runDetail: string | null;
}

/** What the updates band says. */
export interface Band {
  /** `stale` lists what can be updated; `running` a run in flight; `report` a finished one. */
  kind: 'stale' | 'running' | 'report';
  /** The headline, and the pairs or counts under it. */
  line: string;
  detail: string | null;
}

/** One marketplace as its pane draws it. */
export interface MarketplaceRow {
  name: string;
  /** `github` or `directory`, or an empty string when the scan reported none. */
  source: string;
  repo: string | null;
  installLocation: string | null;
  /** The manifest's plugin count. */
  available: number;
  /** The failure reason, or the drift notice, or `null` when healthy. */
  problem: string | null;
  drifted: boolean;
  loadFailed: boolean;
}

const COMPONENT_KINDS: readonly ComponentKind[] = ['skills', 'agents', 'commands', 'hooks', 'lsp'];

function plural(count: number, word: string): string {
  return `${count} ${word}${count === 1 ? '' : 's'}`;
}

/** The display name of a plugin id: the part before `@`. */
export function pluginName(id: string): string {
  const at = id.indexOf('@');
  return at === -1 ? id : id.slice(0, at);
}

/** Every component row of one kind for one plugin, in scan order. */
function componentRows(entry: PluginComponents, kind: ComponentKind): ComponentRow[] {
  switch (kind) {
    case 'skills':
      return entry.skills.map((name) => ({ name, note: null }));
    case 'agents':
      return entry.agents.map((name) => ({ name, note: null }));
    case 'commands':
      return entry.commands.map((name) => ({ name, note: null }));
    case 'hooks':
      return entry.hooks.length === 0 ? [] : [{ name: 'hook set', note: entry.hooks.join(', ') }];
    case 'lsp':
      return Object.keys(entry.lsp_servers).map((server) => ({ name: server, note: null }));
  }
}

function groupLabel(kind: ComponentKind): string {
  return kind;
}

/** A plugin's fold: one group per kind it ships anything of. */
export function foldGroups(entry: PluginComponents, lsp: LspCheck[] = []): ComponentGroup[] {
  const checks = new Map(
    lsp.filter((check) => check.plugin === entry.plugin).map((check) => [check.server, check]),
  );
  return COMPONENT_KINDS.map((kind) => {
    const rows = componentRows(entry, kind).map((row) => {
      const check = kind === 'lsp' ? checks.get(row.name) : undefined;
      // The fold's LSP rows carry the same PATH check the LSP pane does:
      // one vocabulary for one fact, wherever the server is named.
      return check === undefined
        ? row
        : { ...row, note: check.on_path ? `${check.server}: on PATH` : `${check.server}: missing` };
    });
    return { kind, label: groupLabel(kind), count: rows.length, rows };
  }).filter((group) => group.count > 0);
}

/** The counts line for a plugin row. */
function countsLine(entry: PluginComponents): string {
  const parts: string[] = [];
  if (entry.skills.length > 0) parts.push(plural(entry.skills.length, 'skill'));
  if (entry.agents.length > 0) parts.push(plural(entry.agents.length, 'agent'));
  if (entry.commands.length > 0) parts.push(plural(entry.commands.length, 'command'));
  if (entry.hooks.length > 0) parts.push('hooks');
  if (entry.mcp) parts.push('mcp server');
  if (Object.keys(entry.lsp_servers).length > 0) parts.push('lsp');
  return parts.join(' · ');
}

/** Whether the marketplace reports a version this install is not on. */
function isStale(entry: PluginComponents): boolean {
  return (
    entry.installed &&
    entry.enabled &&
    entry.available_version !== null &&
    entry.available_version !== entry.version
  );
}

/** The run row for a plugin id, last write wins. */
function statusFor(run: UpdateRun | null, id: string): RunRow | null {
  if (run === null) return null;
  let found: RunRow | null = null;
  for (const row of run.rows) {
    if (row.plugin_id === id) found = row;
  }
  return found;
}

/**
 * The Installed pane's rows: every installed plugin, and **only** the
 * installed ones - the marketplace catalog renders in its own section
 * below. A registry entry whose directory could not be read still draws,
 * with its reason.
 */
export function pluginRows(wire: ExtensionsRead): PluginRow[] {
  const snapshot = wire.snapshot;
  if (snapshot === null) return [];
  const scopeById = scopesById(wire);
  return snapshot.components
    .filter((entry) => entry.installed)
    .map((entry) => {
      const run = statusFor(wire.update_run, entry.plugin);
      return {
        id: entry.plugin,
        name: pluginName(entry.plugin),
        marketplace: entry.marketplace,
        version: entry.version,
        availableVersion: isStale(entry) ? entry.available_version : null,
        detail: countsLine(entry),
        scope: scopeById.get(entry.plugin) ?? 'user',
        auto: entry.auto,
        enabled: entry.enabled,
        loadError: entry.load_error,
        groups: foldGroups(entry, wire.lsp),
        runStatus: run?.status ?? null,
        runDetail: run?.detail ?? null,
      };
    });
}

/**
 * Each plugin's registry scope, first entry wins.
 *
 * The scope lives on the registry entries, not the component scan, so the
 * two halves are joined by id. A plugin installed in more than one scope
 * has two entries and the first is the one a row acts on - the rows here
 * are one per entry for the same reason, and a pick goes by the row.
 */
function scopesById(wire: ExtensionsRead): Map<string, string> {
  const scopes = new Map<string, string>();
  for (const entry of wire.snapshot?.installed ?? []) {
    if (!scopes.has(entry.id)) scopes.set(entry.id, entry.scope);
  }
  return scopes;
}

/** The mark one plugin row carries. */
export function pluginMark(row: PluginRow): RowMark {
  if (row.runStatus === 'updating') return 'running';
  if (row.runStatus === 'failed') return 'failed';
  if (row.loadError !== null) return 'failed';
  if (row.availableVersion !== null) return 'update';
  return null;
}

/**
 * The plugins with an update waiting - what the band counts and Update all
 * would move. Only enabled installs: a disabled plugin is not updated
 * behind the switch that turned it off.
 */
export function stalePlugins(wire: ExtensionsRead): PluginRow[] {
  return pluginRows(wire).filter((row) => row.availableVersion !== null);
}

/**
 * The band above the list, or `null` when there is nothing to say.
 *
 * A run in flight wins over the stale set (it is the same set, being
 * applied), and a finished manual run leaves its report standing until the
 * next read.
 */
export function band(wire: ExtensionsRead): Band | null {
  const run = wire.update_run;
  if (run !== null && !run.finished) {
    const done = run.rows.filter(
      (row) => row.status === 'updated' || row.status === 'already_current',
    ).length;
    return {
      kind: 'running',
      line: `updating ${plural(run.rows.length, 'plugin')}`,
      detail: done === 0 ? null : `${done} of ${run.rows.length} done`,
    };
  }
  if (run !== null && run.finished && run.trigger === 'manual' && run.rows.length > 0) {
    const updated = run.rows.filter((row) => row.status === 'updated').length;
    const failed = run.rows.filter((row) => row.status === 'failed').length;
    const current = run.rows.filter((row) => row.status === 'already_current').length;
    const checking = run.rows.every((row) => row.status === 'update_available');
    if (checking) {
      return {
        kind: 'report',
        line:
          failed > 0
            ? 'the check could not refresh the inventory'
            : `${plural(run.rows.length, 'plugin')} behind the marketplace`,
        detail: null,
      };
    }
    return {
      kind: 'report',
      line:
        failed > 0
          ? `${updated} updated, ${failed} failed`
          : updated > 0
            ? `${plural(updated, 'plugin')} updated`
            : 'all current',
      detail: current > 0 && updated > 0 ? `${current} already current` : null,
    };
  }
  const stale = stalePlugins(wire);
  if (stale.length === 0) return null;
  const pairs = stale
    .map((row) =>
      row.version !== null && row.availableVersion !== null
        ? `${row.name} ${row.version} → ${row.availableVersion}`
        : row.name,
    )
    .join(' · ');
  const line =
    stale.length === 1
      ? '1 plugin has an update'
      : `${plural(stale.length, 'plugin')} have updates`;
  return { kind: 'stale', line, detail: pairs };
}

/** The rail's counts: installed rows only, so the numbers describe what is here. */
export interface RailCounts {
  installed: number;
  skills: number;
  agents: number;
  commands: number;
  hooks: number;
  lsp: number;
  marketplaces: number;
}

export function railCounts(wire: ExtensionsRead): RailCounts {
  const snapshot: ExtensionsSnapshot | null = wire.snapshot;
  const installed = snapshot?.components.filter((entry) => entry.installed) ?? [];
  const sum = (pick: (entry: PluginComponents) => number): number =>
    installed.reduce((total, entry) => total + pick(entry), 0);
  return {
    installed: installed.length,
    skills: sum((entry) => entry.skills.length),
    agents: sum((entry) => entry.agents.length),
    commands: sum((entry) => entry.commands.length),
    hooks: installed.filter((entry) => entry.hooks.length > 0).length,
    lsp: sum((entry) => Object.keys(entry.lsp_servers).length),
    marketplaces: snapshot?.marketplaces.length ?? 0,
  };
}

/** One component row in a kind's own pane: the name, and the plugin it comes from. */
export interface KindRow {
  name: string;
  /** The plugin's display name. */
  source: string;
  /** The plugin's full id, which an update would name. */
  sourceId: string;
  /** The registry scope the plugin's install sits in. */
  sourceScope: string;
  /** Whether the plugin it belongs to has an update waiting. */
  stale: boolean;
  /** The plugin's row in the live run, when it has one. */
  runStatus: RunRowStatus | null;
  /** For LSP rows: the PATH check's own words. */
  note: string | null;
}

/** The rows one kind's pane draws: every component of installed plugins. */
export function kindRows(wire: ExtensionsRead, kind: ComponentKind): KindRow[] {
  const snapshot = wire.snapshot;
  if (snapshot === null) return [];
  // Keyed by plugin AND server: one plugin may declare several servers, and
  // a check keyed by the plugin alone would hand the first one's result to
  // every row under it. The key here is both.
  const lspChecks = new Map(
    wire.lsp.map((check) => [`${check.plugin}\u0000${check.server}`, check]),
  );
  const scopeById = scopesById(wire);
  const rows: KindRow[] = [];
  for (const entry of snapshot.components) {
    if (!entry.installed) continue;
    const runStatus = statusFor(wire.update_run, entry.plugin)?.status ?? null;
    const base = {
      source: pluginName(entry.plugin),
      sourceId: entry.plugin,
      sourceScope: scopeById.get(entry.plugin) ?? 'user',
      stale: isStale(entry),
      runStatus,
      note: null as string | null,
    };
    if (kind === 'hooks') {
      if (entry.hooks.length > 0) {
        rows.push({ name: pluginName(entry.plugin), ...base, note: entry.hooks.join(', ') });
      }
      continue;
    }
    if (kind === 'lsp') {
      for (const server of Object.keys(entry.lsp_servers)) {
        const check = lspChecks.get(`${entry.plugin}\u0000${server}`);
        const note =
          check === undefined
            ? null
            : check.on_path
              ? `${check.server}: on PATH`
              : `${check.server}: missing`;
        rows.push({ name: server, ...base, note });
      }
      continue;
    }
    const names =
      kind === 'skills' ? entry.skills : kind === 'agents' ? entry.agents : entry.commands;
    for (const name of names) rows.push({ name, ...base });
  }
  return rows;
}

/** One plugin that can be installed, as the marketplaces section draws it. */
export interface AvailableRow {
  id: string;
  name: string;
  marketplace: string;
  version: string | null;
  /** What it ships: `skills · commands`, `lsp server`, ... */
  detail: string;
  description: string | null;
}

/**
 * The plugins that are in a marketplace and not installed here.
 *
 * The scan's own components list is the source: an entry the scan could not
 * read a version for still draws (with no version), because a catalog row
 * that vanished would read as a marketplace that lost its plugin.
 */
export function availableRows(wire: ExtensionsRead): AvailableRow[] {
  const snapshot = wire.snapshot;
  if (snapshot === null) return [];
  const described = new Map(snapshot.marketplace.map((entry) => [entry.plugin_id, entry]));
  return snapshot.components
    .filter((entry) => !entry.installed)
    .map((entry) => ({
      id: entry.plugin,
      name: pluginName(entry.plugin),
      marketplace: entry.marketplace,
      version: entry.version ?? described.get(entry.plugin)?.version ?? null,
      detail: countsLine(entry),
      description: described.get(entry.plugin)?.description ?? null,
    }));
}

/** The marketplace rows: the configured sources joined with their health. */
export function marketplaceRows(wire: ExtensionsRead): MarketplaceRow[] {
  const snapshot = wire.snapshot;
  if (snapshot === null) return [];
  const health = new Map(snapshot.marketplace_health.map((entry) => [entry.name, entry]));
  return snapshot.marketplaces.map((entry) => {
    const report = health.get(entry.name);
    const drifted = report?.drifted === true;
    const loadFailed = report?.load_error != null;
    return {
      name: entry.name,
      source: entry.source ?? '',
      repo: entry.repo,
      installLocation: entry.install_location,
      available: report?.available ?? 0,
      problem: loadFailed
        ? report.load_error
        : drifted
          ? 'registry drift - install location outside the config dir'
          : null,
      drifted,
      loadFailed,
    };
  });
}

/** A marketplace's own line: what its source is and how many plugins it carries. */
export function marketplaceDetail(row: MarketplaceRow): string {
  const where =
    row.source === 'directory'
      ? (row.installLocation ?? row.repo ?? 'directory')
      : (row.repo ?? row.source);
  const count = row.loadFailed ? 'cannot load' : plural(row.available, 'plugin');
  return where === '' ? count : `${where} · ${count}`;
}
