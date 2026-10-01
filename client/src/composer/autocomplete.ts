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

import { MIN_QUERY_CHARS, matches as emojiMatches, shortcodeQuery } from './emoji';
import { isForgeCommand } from './forge-commands';
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

/** The two sources the `/` list draws from, which is also the order it offers them in. */
const SOURCES = ['forge', 'cli'] as const;

/** Which source a command came from, which is the heading it is drawn under. */
type Source = 0 | 1;

/** One command with the source it came from. */
interface Sourced {
  command: Advisory;
  source: Source;
}

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
  /** The heading this row opens, or `null` when it opens none. */
  group: string | null;
}

/** A run of rows under one heading, which is how a listbox draws its groups. */
export interface Group {
  /** What the list draws above these rows, or `null` when it heads none. */
  title: string | null;
  /** The index of `rows[0]` in the offer's flat list, which is what a mark counts in. */
  from: number;
  rows: Row[];
}

/** Which list a draft opened, and what it holds. */
export interface Offer {
  kind: 'command' | 'file' | 'agent' | 'emoji';
  /** Where the token starts in the draft, which a pick replaces from. */
  from: number;
  query: string;
  /** How many rows matched, which is what the header states. */
  total: number;
  /** Every row in the order it is offered, which is the list a mark indexes into. */
  rows: Row[];
  /** The same rows cut where a heading goes. */
  groups: Group[];
}

/** One list's own title and icon, which the header draws. */
export const HEADINGS: Record<Offer['kind'], { icon: string; title: string }> = {
  command: { icon: 'cmd', title: 'commands' },
  file: { icon: 'file', title: 'files & folders' },
  agent: { icon: 'bot', title: 'subagents' },
  emoji: { icon: 'smile', title: 'emoji' },
};

/**
 * The id the popup carries, which the field points its `aria-controls` at.
 *
 * One list at a time, so one id: a popup is the field's, and the field is what
 * owns the keyboard while it is open - the combobox pattern, rather than a
 * listbox the reader would have to Tab into.
 */
export const LIST_ID = 'composer-list';

/** The id one row carries, which the field points at while a key moves the mark. */
export function rowId(offer: Offer, at: number): string {
  return `ac-${offer.kind}-${at}`;
}

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
 *
 * `sources` is a pull because the record it comes from is replaced on every
 * frame the server sends, so a draft that opens nothing must not rebuild a
 * list it will not draw - the file index among them is the whole working tree.
 * All four are built together on the pull, so what a list reads stays what the
 * derived offer depends on.
 */
export function offer(draft: string, sources: () => Sources): Offer | null {
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
  const found = ranked(query);
  if (found.length === 0) return null;
  const rows = found.slice(0, CANDIDATES);
  return { kind, from, query, total: found.length, rows, groups: grouped(rows) };
}

/** The rows cut where a heading goes, which is the shape a listbox carries its groups in. */
function grouped(rows: Row[]): Group[] {
  const found: Group[] = [];
  for (const [at, row] of rows.entries()) {
    const last = found[found.length - 1];
    if (row.group !== null || last === undefined) {
      found.push({ title: row.group, from: at, rows: [row] });
      continue;
    }
    last.rows.push(row);
  }
  return found;
}

/** The emoji list, whose token rule is the shortcode one rather than a prefix. */
function emojiOffer(draft: string): Offer | null {
  const query = shortcodeQuery(draft);
  if (query === null || query.length < MIN_QUERY_CHARS) return null;
  const found = emojiMatches(query);
  if (found.length === 0) return null;
  const rows = found.slice(0, CANDIDATES).map((emoji) => ({
    insert: emoji.glyph,
    text: `:${emoji.name}:`,
    detail: '',
    glyph: emoji.glyph,
    group: null,
  }));
  return {
    kind: 'emoji',
    from: draft.length - query.length - 1,
    query,
    total: found.length,
    rows,
    groups: grouped(rows),
  };
}

/**
 * forge's commands first, then the CLI's, with a name in both counted once: the
 * source settles a tie between two rows that matched equally well, and heads
 * each group while the list is whole.
 */
function commands(sources: () => Sources): (query: string) => Row[] {
  const { forgeCommands, advertised } = sources();
  const cli = advertised.filter((command) => !isForgeCommand(command.name));
  const held: Sourced[] = [
    ...forgeCommands.map((command): Sourced => ({ command, source: 0 })),
    ...cli.map((command): Sourced => ({ command, source: 1 })),
  ];
  return (query) =>
    rank(held, query).map((row, at, all) => ({
      insert: row.command.name,
      text: row.command.name,
      detail: row.command.description,
      glyph: null,
      group: query !== '' || all[at - 1]?.source === row.source ? null : SOURCES[row.source],
    }));
}

/**
 * The files `query` matches, best first: a basename match leads a path match,
 * the shallower of two leads the deeper, and the alphabet settles the rest.
 */
function files(sources: () => Sources): (query: string) => Row[] {
  const { files: held } = sources();
  return (query) => {
    const folded = query.toLowerCase();
    const matched = held
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
      group: null,
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
function agents(sources: () => Sources): (query: string) => Row[] {
  const { agents: held } = sources();
  return (query) =>
    held
      .filter((agent) => matches([agent.name, agent.description], query))
      .map((agent) => ({
        insert: `&${agent.name}`,
        text: agent.name,
        detail: agent.description,
        glyph: null,
        group: null,
      }));
}

/** Whether any of a row's own fields is what has been typed. */
function matches(fields: string[], query: string): boolean {
  const folded = query.toLowerCase();
  return fields.some((field) => field.toLowerCase().includes(folded));
}

/**
 * The commands whose own text is what has been typed, best first: the exact
 * name, then the ones that start with it, then the rest that contain it, and
 * forge's own ahead of the CLI's wherever two matched equally well.
 */
function rank(held: Sourced[], query: string): Sourced[] {
  const folded = query.toLowerCase();
  const scored: { rank: number; name: string; row: Sourced }[] = [];
  for (const row of held) {
    // Ranked on the name WITHOUT its slash: the query is what came after the
    // trigger, and a slash at the front of the name would make every command a
    // substring match rather than a prefix one.
    const names = [row.command.name.slice(1), row.command.description].map((field) =>
      field.toLowerCase(),
    );
    const name = names[0] ?? '';
    const rank =
      name === folded
        ? 0
        : name.startsWith(folded)
          ? 1
          : names.some((f) => f.includes(folded))
            ? 2
            : -1;
    if (rank >= 0) scored.push({ rank, name, row });
  }
  scored.sort(
    (a, b) => a.rank - b.rank || a.row.source - b.row.source || a.name.localeCompare(b.name),
  );
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

/**
 * The offset that keeps a marked row inside its window and moves it no further:
 * the terminal's own rule, whose picker windows the list around the mark
 * (`DialogState::clamp` in `crates/forge-tui`).
 *
 * `row` is where the row sits in the window: the rectangle's own edges less the
 * window's top.
 */
export function keptInView(
  row: { top: number; bottom: number },
  height: number,
  scrollTop: number,
): number {
  if (row.top < 0) return scrollTop + row.top;
  if (row.bottom > height) return scrollTop + row.bottom - height;
  return scrollTop;
}
