/**
 * Which family a call belongs to, and the word its row carries.
 *
 * **This vocabulary is the fold's, and it is the only one.** The grouping was
 * the server's until it moved here, and the table below is that table moved
 * rather than a second naming of it: the chat and the inspector describe the
 * same tool with the same word or they describe it two ways.
 *
 * A family is a concept and a glyph is one rendering of it, so the sprite a
 * row draws is chosen here from the family and never stored beside it.
 */

/** The group a call's row is summarised under. */
export type Family =
  | 'read'
  | 'search'
  | 'bash'
  | 'web'
  | 'lsp'
  | 'skill'
  | 'toolsearch'
  | 'config'
  | 'worktree'
  | 'tool'
  /** Every mutation, which the fold gives one row whatever tool it was. */
  | 'edit';

/**
 * The forge MCP families whose results draw as a card of their own.
 *
 * One name per submodule of the `forge` server rather than a second list: the
 * family segment a tool name carries IS the family (`mcp__forge__tasks__update`
 * is tasks), so a family added on the server arrives here by being named.
 */
export type ForgeFamily = 'agents' | 'cron' | 'gotify' | 'review' | 'slack' | 'tasks';

/** The families a `forge` call draws a card for; `systemone` keeps its own kind. */
const FORGE_FAMILIES = new Set<string>(['agents', 'cron', 'gotify', 'review', 'slack', 'tasks']);

/**
 * The forge family a tool belongs to, or `null` for every other call.
 *
 * A tool name is `mcp__<server>__<family>__<verb>` for every family but the
 * decisions, so the family is the first segment under the server - read from
 * the name rather than matched against a list of 30 tool names, because the
 * verb is the card's business and the family is not.
 */
export function forgeFamilyOf(name: string): ForgeFamily | null {
  const parts = mcpParts(name);
  if (parts === null || parts.server !== 'forge') return null;
  const cut = parts.tool.indexOf('__');
  if (cut <= 0) return null;
  const family = parts.tool.slice(0, cut);
  return FORGE_FAMILIES.has(family) ? (family as ForgeFamily) : null;
}

/**
 * The class a row belongs to, which is what a view picks its glyph from.
 *
 * A label alone cannot tell a server named `read` from the read family, which
 * is why a server row and a family row are told apart here rather than by
 * comparing words.
 */
export type KindRow =
  | { kind: 'family'; family: Family }
  | { kind: 'forge'; family: ForgeFamily }
  | { kind: 'mcp' }
  | { kind: 'systemone' }
  | { kind: 'inbound' }
  | { kind: 'outbound' };

/** A call's status, as the wire writes it. */
export type CallStatus = 'pending' | 'in_progress' | 'completed' | 'failed' | 'killed';

/** The four tools the fold draws as one `edit` row. */
const MUTATIONS = new Set(['Edit', 'Write', 'MultiEdit', 'NotebookEdit']);

/** Whether a call is a mutation. */
export function isEdit(name: string): boolean {
  return MUTATIONS.has(name);
}

/**
 * The System One decisions, which draw as a family of their own rather than
 * under the `forge` server every other MCP tool of forge's shares: a reader
 * scanning for what the session decided must not find the three mixed among
 * its cron and peer calls.
 */
const DECISION_TOOLS = new Set([
  'mcp__forge__systemone__ask_noul',
  'mcp__forge__systemone__ask_choice',
  'mcp__forge__systemone__ask_score',
]);

/** Whether a call is one of the System One decisions. */
export function isDecisionTool(name: string): boolean {
  return DECISION_TOOLS.has(name);
}

/** A server-side tool's own parts, for a name shaped `mcp__<server>__<tool>`. */
export function mcpParts(name: string): { server: string; tool: string } | null {
  if (!name.startsWith('mcp__')) return null;
  const rest = name.slice('mcp__'.length);
  const cut = rest.indexOf('__');
  if (cut <= 0) return null;
  return { server: rest.slice(0, cut), tool: rest.slice(cut + 2) };
}

/** The family a tool name belongs to. */
export function familyOf(name: string): Family {
  if (isEdit(name)) return 'edit';
  const named: Record<string, Family> = {
    Read: 'read',
    Glob: 'search',
    Grep: 'search',
    LS: 'search',
    Bash: 'bash',
    WebFetch: 'web',
    web_fetch: 'web',
    WebSearch: 'web',
    web_search: 'web',
    LSP: 'lsp',
    Skill: 'skill',
    advisor: 'skill',
    ToolSearch: 'toolsearch',
    tool_search_tool_regex: 'toolsearch',
    tool_search_tool_bm25: 'toolsearch',
    ExitPlanMode: 'config',
    EnterPlanMode: 'config',
    Config: 'config',
    Move: 'worktree',
    EnterWorktree: 'worktree',
    ExitWorktree: 'worktree',
  };
  return named[name] ?? 'tool';
}

/** The row a call is summarised under. */
export function rowOf(name: string): KindRow {
  if (isDecisionTool(name)) return { kind: 'systemone' };
  const family = forgeFamilyOf(name);
  if (family !== null) return { kind: 'forge', family };
  if (mcpParts(name) !== null) return { kind: 'mcp' };
  return { kind: 'family', family: familyOf(name) };
}

/**
 * The sprite a forge family draws.
 *
 * Every one is a shipped id except `review`, which is this work's own addition:
 * the mock's Lucide git-pull-request, added to the sprite in the PR that first
 * draws a review card.
 */
const FORGE_GLYPH: Record<ForgeFamily, string> = {
  agents: 'subagents',
  cron: 'schedules',
  gotify: 'gotify',
  review: 'review',
  slack: 'slack',
  tasks: 'tasks',
};

/**
 * The sprite each family draws.
 *
 * Three families draw a symbol of a nearer name than their own, because the
 * family words name no sprite: bash is the boxed terminal, config the settings
 * gear, and worktree a git branch.
 */
const FAMILY_GLYPH: Record<Family, string> = {
  read: 'read',
  search: 'search',
  bash: 'square-terminal',
  web: 'web',
  lsp: 'lsp',
  skill: 'skill',
  toolsearch: 'toolsearch',
  config: 'settings',
  worktree: 'git',
  tool: 'tool',
  edit: 'edit',
};

/** The sprite a row draws, by the class it belongs to. */
export function iconOf(row: KindRow): string {
  switch (row.kind) {
    case 'forge':
      return FORGE_GLYPH[row.family];
    case 'mcp':
      return 'mcp';
    case 'systemone':
      return 'decide';
    case 'inbound':
    case 'outbound':
      return 'in';
    case 'family':
      return FAMILY_GLYPH[row.family];
  }
}

/**
 * A task frame's own status word, as the call it belongs to is drawn.
 *
 * The same four words the terminal maps to the same statuses: the wire says
 * `running` where the row says in progress, and `stopped` is its word for a
 * graceful cancel, which draws as the kill it is.
 *
 * **And one place it deliberately differs from the terminal.** An unrecognised
 * word is `null` here rather than the terminal's `Pending`, so the caller
 * keeps the status the call already had: a task whose update the page cannot
 * read is a task it does not know has ended, and drawing it as pending again
 * would walk a finished call back down.
 */
export function taskStatus(wire: string | null): CallStatus | null {
  switch (wire) {
    case 'running':
      return 'in_progress';
    case 'completed':
      return 'completed';
    case 'failed':
      return 'failed';
    case 'killed':
    case 'stopped':
      return 'killed';
    default:
      return null;
  }
}
