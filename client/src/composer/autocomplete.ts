/**
 * The autocomplete: which list a draft opens, what it holds, and the order it
 * offers it in.
 *
 * Four triggers, one popover shape. Two of the four lists are the core's own
 * reads for this seat - the commands the CLI advertised and the subagent types
 * it names - and two are static: forge's command table and the emoji set, both
 * of which ship in the bundle rather than crossing the wire.
 *
 * **The ordering is this file's, not the server's.** Which match a typeahead
 * offers first is ordering-for-display, so it lives with the view that draws it:
 * the emoji ranking comes from `crates/forge-tui/src/app/emoji.rs`, and the file
 * ranking from `crates/forge-server/src/file_index.rs`. The trigger rules are
 * the terminal's own, ported rather than reinvented - a slash command is the
 * whole draft while it is typed, and the `:` counts only at the start of the
 * text or after whitespace.
 */

import { MIN_QUERY_CHARS, TABLE as EMOJI, matches as emojiMatches, shortcodeQuery } from './emoji';
import type { Advisory, AgentType, FileEntry } from './wire';

/**
 * How many candidates a list is ranked down to before it is drawn.
 *
 * A bound on WORK rather than a number of rows: what that many rows cost to
 * rank is the thing being capped, and the list itself scrolls in a window the
 * popover's own height sets. The header states the number of matches, so a list
 * cut here never reads as the whole list.
 */
export const CANDIDATES = 200;

/** What a draft is matched against, which is the seat's own reads plus the bundle. */
export interface Sources {
  /** forge's own table, which shadows the CLI's rows for the same name. */
  forgeCommands: Advisory[];
  /** The commands the CLI last advertised for this seat. */
  advertised: Advisory[];
  /** This seat's files, keyed by their path from the session's own root. */
  files: FileEntry[];
  /** The subagent types the CLI catalogue names. */
  agents: AgentType[];
}

/** One row of a list: what it writes, what it reads as, and the icon it carries. */
export interface Row {
  /** The value a pick writes into the draft, replacing the token it opened on. */
  insert: string;
  /** The primary text, which the list marks the query inside. */
  text: string;
  /** What sits beside it, dim, when the row has anything to add. */
  detail: string;
  /** The glyph column, which only the emoji list fills. */
  glyph: string | null;
}

/** Which list a draft opened, and what it holds. */
export interface Offer {
  kind: 'command' | 'file' | 'agent' | 'emoji';
  /** Where the token starts in the draft, which a pick replaces from. */
  from: number;
  query: string;
  /** How many rows matched, which is what the header states. */
  total: number;
  rows: Row[];
}

/** One list's own title and icon, which the header draws. */
export const HEADINGS: Record<Offer['kind'], { icon: string; title: string }> = {
  command: { icon: 'cmd', title: 'commands' },
  file: { icon: 'file', title: 'files & folders' },
  agent: { icon: 'bot', title: 'subagents' },
  emoji: { icon: 'smile', title: 'emoji' },
};

/** The token a draft ends in, and where it starts. */
function token(draft: string): { text: string; from: number } {
  const found = /\S+$/.exec(draft);
  if (found === null) return { text: '', from: draft.length };
  return { text: found[0], from: found.index };
}

/**
 * The list `draft` opens, or `null` when it opens none.
 *
 * A list with no rows opens nothing: a popover holding only a header reads as a
 * list that lost its rows rather than as a query that matched nothing.
 */
export function offer(draft: string, sources: Sources): Offer | null {
  const { text: last, from } = token(draft);
  if (last.startsWith('/') && last === draft) {
    return of('command', 0, last.slice(1), commands(sources));
  }
  if (last.startsWith('@') && last.length > 1) {
    return of('file', from, last.slice(1), files(sources));
  }
  if (last.startsWith('&') && last.length > 1) {
    return of('agent', from, last.slice(1), agents(sources));
  }
  return emojiOffer(draft);
}

function of(
  kind: Offer['kind'],
  from: number,
  query: string,
  ranked: (query: string) => Row[],
): Offer | null {
  const rows = ranked(query);
  if (rows.length === 0) return null;
  return { kind, from, query, total: rows.length, rows: rows.slice(0, CANDIDATES) };
}

/** The emoji list, whose token rule is the shortcode one rather than a prefix. */
function emojiOffer(draft: string): Offer | null {
  const query = shortcodeQuery(draft);
  if (query === null || query.length < MIN_QUERY_CHARS) return null;
  const found = emojiMatches(query);
  if (found.length === 0) return null;
  return {
    kind: 'emoji',
    from: draft.length - query.length - 1,
    query,
    total: found.length,
    rows: found.slice(0, CANDIDATES).map((emoji) => ({
      insert: emoji.glyph,
      text: `:${emoji.name}:`,
      detail: '',
      glyph: emoji.glyph,
    })),
  };
}

/** forge's commands first, then the CLI's, with a name in both counted once. */
function commands(sources: Sources): (query: string) => Row[] {
  const merged: Advisory[] = [
    ...sources.forgeCommands,
    ...sources.advertised.filter((command) => !isForge(command.name, sources.forgeCommands)),
  ];
  // Ranked on the name WITHOUT its slash: the query is what came after the
  // trigger, and a slash at the front of the name would make every command a
  // substring match rather than a prefix one.
  return (query) =>
    rank(merged, query, (command) => [command.name.slice(1), command.description]).map(
      (command) => ({
        insert: command.name,
        text: command.name,
        detail: command.description,
        glyph: null,
      }),
    );
}

function isForge(name: string, forge: Advisory[]): boolean {
  return forge.some((command) => command.name === name);
}

/**
 * The files `query` matches, best first: a basename match leads a path match,
 * the shallower of two leads the deeper, and the alphabet settles the rest.
 */
function files(sources: Sources): (query: string) => Row[] {
  return (query) => {
    const folded = query.toLowerCase();
    const matched = sources.files
      .map((file) => ({ file, tier: fileTier(file, folded) }))
      .filter((entry) => entry.tier !== null);
    matched.sort(
      (a, b) =>
        (a.tier ?? 0) - (b.tier ?? 0) ||
        a.file.depth - b.file.depth ||
        a.file.relPath.localeCompare(b.file.relPath),
    );
    return matched.map((entry) => ({
      insert: `@${entry.file.relPath}`,
      text: entry.file.relPath,
      detail: '',
      glyph: null,
    }));
  };
}

/** How well a file matches, smaller being better, or `null` when it does not. */
function fileTier(file: FileEntry, query: string): number | null {
  if (file.basenameLower.startsWith(query)) return 0;
  if (file.relPathLower.startsWith(query)) return 1;
  if (file.basenameLower.includes(query)) return 2;
  if (file.relPathLower.includes(query)) return 3;
  return null;
}

/**
 * The subagent types `query` matches, on their name or what they do.
 *
 * Filtered rather than ranked: a catalogue is short and the CLI advertises it
 * in its own order, so which of two matches comes first is the catalogue's
 * answer and not one this list invents.
 */
function agents(sources: Sources): (query: string) => Row[] {
  return (query) =>
    sources.agents
      .filter((agent) => matches([agent.name, agent.description], query))
      .map((agent) => ({
        insert: `&${agent.name}`,
        text: agent.name,
        detail: agent.description,
        glyph: null,
      }));
}

/** Whether any of a row's own fields is what has been typed. */
function matches(fields: string[], query: string): boolean {
  const folded = query.toLowerCase();
  return fields.some((field) => field.toLowerCase().includes(folded));
}

/**
 * The rows whose own text is what has been typed, best first: the exact name,
 * then the ones that start with it, then the rest that contain it.
 *
 * A list is filtered and never left whole, or a query would look like it did
 * nothing.
 */
function rank<T>(held: T[], query: string, fields: (row: T) => string[]): T[] {
  const folded = query.toLowerCase();
  const scored: { rank: number; name: string; row: T }[] = [];
  for (const row of held) {
    const names = fields(row).map((field) => field.toLowerCase());
    const name = names[0] ?? '';
    const rank =
      name === folded ? 0 : name.startsWith(folded) ? 1 : names.some((f) => f.includes(folded)) ? 2 : -1;
    if (rank >= 0) scored.push({ rank, name, row });
  }
  scored.sort((a, b) => a.rank - b.rank || a.name.localeCompare(b.name));
  return scored.map((entry) => entry.row);
}

/**
 * `text` split around the query's own span, which is what the list marked it on.
 *
 * Nothing is marked when the query is empty, which is what a bare trigger
 * shows. A query whose lowercase changes the length cannot be found by offset,
 * and marking the wrong span is worse than marking none.
 */
export function mark(text: string, query: string): { before: string; hit: string; after: string } {
  const lower = text.toLowerCase();
  const hits = lower.length === text.length && query !== '';
  const at = hits ? lower.indexOf(query.toLowerCase()) : -1;
  if (at < 0) return { before: text, hit: '', after: '' };
  const end = at + query.length;
  return { before: text.slice(0, at), hit: text.slice(at, end), after: text.slice(end) };
}

/** How many shortcodes the set holds, which the emoji list's header used to state. */
export const EMOJI_COUNT = EMOJI.length;
