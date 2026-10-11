/**
 * The Extensions page's read, as `crates/forge-server/src/transport/wire.rs`
 * writes it and `Subject::Extensions` answers with.
 *
 * **`snapshot` and `update_run` are the same shapes the live frames carry**:
 * `plugins_inventory_updated` embeds the snapshot object and
 * `plugins_update_run_progress` / `_finished` embed the run, so a page that
 * took one off the stream and a page that read it are drawing the same
 * thing. `lsp` is read-only - the PATH checks are the server's, and a page
 * re-reads to refresh them.
 *
 * Every union here is narrowed once (see `extensionsFrom`), so the page
 * switches over them exhaustively and a variant the server adds is a
 * compile error rather than a row that silently draws as something else.
 */

/** One registry entry: what is installed, in which scope. */
export interface InstalledPlugin {
  id: string;
  version: string | null;
  scope: string;
  enabled: boolean;
  project_path: string | null;
}

/** One marketplace catalog entry: a plugin that can be installed. */
export interface MarketplacePlugin {
  plugin_id: string;
  name: string;
  description: string | null;
  marketplace_name: string | null;
  version: string | null;
}

/** One configured marketplace. */
export interface MarketplaceSource {
  name: string;
  /** `github` or `directory`, as the CLI reports it. */
  source: string | null;
  repo: string | null;
  install_location: string | null;
}

/**
 * One plugin's component inventory, read off disk: the scan's own unit and
 * what the page's rows are built from.
 */
export interface PluginComponents {
  /** The full id, `name@marketplace`. */
  plugin: string;
  marketplace: string;
  version: string | null;
  installed: boolean;
  enabled: boolean;
  /** The registry marks the install as an auto-installed dependency. */
  auto: boolean;
  /** The latest version the marketplace manifest declares. */
  available_version: string | null;
  skills: string[];
  agents: string[];
  commands: string[];
  /** Hook trigger events (`SessionStart`, ...). */
  hooks: string[];
  mcp: boolean;
  /** LSP servers the manifest declares: server key to the command it launches. */
  lsp_servers: Record<string, string>;
  /** Why the row could not load: a registry install whose directory is gone, or a manifest that failed. */
  load_error: string | null;
}

/** One marketplace's on-disk health. */
export interface MarketplaceHealth {
  name: string;
  source: string;
  /** Plugin count in the marketplace's manifest; 0 when it cannot load. */
  available: number;
  load_error: string | null;
  /** The registry's install location sits outside the config dir. */
  drifted: boolean;
}

/** The inventory half of the read: what the scan found. */
export interface ExtensionsSnapshot {
  installed: InstalledPlugin[];
  marketplace: MarketplacePlugin[];
  marketplaces: MarketplaceSource[];
  components: PluginComponents[];
  marketplace_health: MarketplaceHealth[];
}

/** Where one row of an update run or check got to. */
export type RunRowStatus =
  | 'queued'
  | 'updating'
  | 'updated'
  | 'already_current'
  | 'failed'
  | 'skipped'
  | 'update_available'
  | 'other';

/** One row of a run: the plugin, its scope, and what happened to it. */
export interface RunRow {
  plugin_id: string;
  scope: string;
  status: RunRowStatus;
  installed_version: string | null;
  available_version: string | null;
  /** The failure text for `failed`, or the reason for `skipped`. */
  detail: string | null;
}

/** The last update run or check, in flight or finished. */
export interface UpdateRun {
  trigger: 'manual' | 'auto' | 'other';
  finished: boolean;
  rows: RunRow[];
}

/** One declared LSP server and whether its binary resolves on PATH. */
export interface LspCheck {
  plugin: string;
  server: string;
  command: string;
  on_path: boolean;
}

/** The read as the page consumes it. */
export interface ExtensionsRead {
  /** `null` when no plugins root resolves: the inventory is unknowable, which is its own state. */
  snapshot: ExtensionsSnapshot | null;
  update_run: UpdateRun | null;
  lsp: LspCheck[];
}

/** The statuses the server writes; the eighth is this client's own. */
const RUN_STATUSES: Exclude<RunRowStatus, 'other'>[] = [
  'queued',
  'updating',
  'updated',
  'already_current',
  'failed',
  'skipped',
  'update_available',
];

const TRIGGERS = ['manual', 'auto'] as const;

function narrow<T extends string>(value: unknown, known: readonly T[], fallback: T): T {
  return typeof value === 'string' && (known as readonly string[]).includes(value)
    ? (value as T)
    : fallback;
}

function str(value: unknown): string | null {
  return typeof value === 'string' ? value : null;
}

function num(value: unknown): number | null {
  return typeof value === 'number' ? value : null;
}

function strings(value: unknown): string[] {
  return Array.isArray(value)
    ? value.filter((item): item is string => typeof item === 'string')
    : [];
}

function objects(value: unknown): Record<string, unknown>[] {
  return Array.isArray(value)
    ? value.filter(
        (item): item is Record<string, unknown> => item !== null && typeof item === 'object',
      )
    : [];
}

/**
 * The read as it crossed, with every union and every absent field narrowed
 * HERE and not in a renderer.
 *
 * A field this client is older than arrives absent rather than wrong: the
 * scan plugin predates nothing today, but a server older than this client
 * would answer a subject it does not carry with an error rather than a
 * short object, so absence here means a shape that surprised us - each
 * reader gets a value that draws as "unknown" rather than as a claim.
 */
export function extensionsFrom(data: unknown): ExtensionsRead {
  const record = (data ?? {}) as Record<string, unknown>;
  const snapshot = record['snapshot'];
  return {
    snapshot:
      snapshot === null || typeof snapshot !== 'object'
        ? null
        : snapshotFrom(snapshot as Record<string, unknown>),
    update_run: runFrom(record['update_run']),
    lsp: objects(record['lsp']).map((check) => ({
      plugin: str(check['plugin']) ?? '',
      server: str(check['server']) ?? '',
      command: str(check['command']) ?? '',
      on_path: check['on_path'] === true,
    })),
  };
}

function snapshotFrom(snapshot: Record<string, unknown>): ExtensionsSnapshot {
  return {
    installed: objects(snapshot['installed']).map((entry) => ({
      id: str(entry['id']) ?? '',
      version: str(entry['version']),
      scope: str(entry['scope']) ?? 'user',
      enabled: entry['enabled'] !== false,
      project_path: str(entry['project_path']),
    })),
    marketplace: objects(snapshot['marketplace']).map((entry) => ({
      plugin_id: str(entry['plugin_id']) ?? '',
      name: str(entry['name']) ?? '',
      description: str(entry['description']),
      marketplace_name: str(entry['marketplace_name']),
      version: str(entry['version']),
    })),
    marketplaces: objects(snapshot['marketplaces']).map((entry) => ({
      name: str(entry['name']) ?? '',
      source: str(entry['source']),
      repo: str(entry['repo']),
      install_location: str(entry['install_location']),
    })),
    components: objects(snapshot['components']).map((entry) => ({
      plugin: str(entry['plugin']) ?? '',
      marketplace: str(entry['marketplace']) ?? '',
      version: str(entry['version']),
      installed: entry['installed'] === true,
      enabled: entry['enabled'] !== false,
      auto: entry['auto'] === true,
      available_version: str(entry['available_version']),
      skills: strings(entry['skills']),
      agents: strings(entry['agents']),
      commands: strings(entry['commands']),
      hooks: strings(entry['hooks']),
      mcp: entry['mcp'] === true,
      lsp_servers: lspServers(entry['lsp_servers']),
      load_error: str(entry['load_error']),
    })),
    marketplace_health: objects(snapshot['marketplace_health']).map((entry) => ({
      name: str(entry['name']) ?? '',
      source: str(entry['source']) ?? '',
      available: num(entry['available']) ?? 0,
      load_error: str(entry['load_error']),
      drifted: entry['drifted'] === true,
    })),
  };
}

function lspServers(value: unknown): Record<string, string> {
  if (value === null || typeof value !== 'object') return {};
  const out: Record<string, string> = {};
  for (const [server, command] of Object.entries(value as Record<string, unknown>)) {
    if (typeof command === 'string') out[server] = command;
  }
  return out;
}

/** One run as a live frame carries it, narrowed the way the read's is. */
export function updateRunFrom(value: unknown): UpdateRun | null {
  return runFrom(value);
}

function runFrom(value: unknown): UpdateRun | null {
  if (value === null || typeof value !== 'object') return null;
  const run = value as Record<string, unknown>;
  return {
    trigger: narrow(run['trigger'], TRIGGERS, 'other'),
    finished: run['finished'] === true,
    rows: objects(run['rows']).map((row) => ({
      plugin_id: str(row['plugin_id']) ?? '',
      scope: str(row['scope']) ?? 'user',
      status: narrow(row['status'], RUN_STATUSES, 'other'),
      installed_version: str(row['installed_version']),
      available_version: str(row['available_version']),
      detail: str(row['detail']),
    })),
  };
}
