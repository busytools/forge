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
 * The class a row belongs to, which is what a view picks its glyph from.
 *
 * A label alone cannot tell a server named `read` from the read family, which
 * is why a server row and a family row are told apart here rather than by
 * comparing words.
 */
export type KindRow =
  { kind: 'family'; family: Family } | { kind: 'mcp' } | { kind: 'inbound' } | { kind: 'outbound' };

/** A call's status, as the wire writes it. */
export type CallStatus = 'pending' | 'in_progress' | 'completed' | 'failed' | 'killed';

/** The four tools the fold draws as one `edit` row. */
const MUTATIONS = new Set(['Edit', 'Write', 'MultiEdit', 'NotebookEdit']);

/** Whether a call is a mutation. */
export function isEdit(name: string): boolean {
  return MUTATIONS.has(name);
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
  if (mcpParts(name) !== null) return { kind: 'mcp' };
  return { kind: 'family', family: familyOf(name) };
}

/**
 * The word a row draws.
 *
 * A family draws its own name, a mutation draws `edit` whatever tool it was,
 * and a server draws its own name rather than a generic word: two servers are
 * two lanes, and the lane a call sits in says which one ran it.
 */
export function labelOf(name: string): string {
  const mcp = mcpParts(name);
  if (mcp !== null) return mcp.server;
  if (isEdit(name)) return 'edit';
  return familyOf(name);
}

/** The sprite a row draws, by the class it belongs to. */
export function iconOf(row: KindRow): string {
  switch (row.kind) {
    case 'mcp':
      return 'mcp';
    case 'inbound':
    case 'outbound':
      return 'in';
    case 'family':
      return row.family === 'edit' ? 'edit' : row.family;
  }
}

/**
 * What a run of calls reports.
 *
 * The roll-up says the run has a failure in it and never which call: the
 * per-call status is what says that, and it rides every row. A run still going
 * is in progress whatever else is in it, because the thing a reader wants to
 * know about a run in flight is that it is in flight.
 */
export function aggregateStatus(statuses: readonly CallStatus[]): CallStatus {
  let failed = false;
  let pending = false;
  for (const status of statuses) {
    if (status === 'in_progress') return 'in_progress';
    if (status === 'failed' || status === 'killed') failed = true;
    if (status === 'pending') pending = true;
  }
  if (failed) return 'failed';
  return pending ? 'pending' : 'completed';
}
