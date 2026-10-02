/**
 * The text a conversation carries, as a page draws it.
 *
 * **Stripping escape sequences is the client's, not the server's.** A
 * terminal's escapes are a rendering of text that has none, so the server
 * sends what the command produced and the page takes the escapes off. It is
 * not a cosmetic step: a browser has no terminal to obey a CSI, so what it
 * draws is the RESIDUE - and an unterminated sequence eats the text after it,
 * which leaves the reader a truncated command rather than a styled one.
 *
 * The sequences are scanned rather than matched, because the interesting case
 * is the one that never terminates: a pattern that requires a terminator
 * leaves most of a truncated sequence on the page, which is the failure this
 * exists to prevent. A scan also reads as what it is, where a regular
 * expression over control characters reads as punctuation.
 */

/** What a terminal acts on rather than prints. */
const ESCAPE = 27;
const BELL = 7;
const TAB = 9;
const LINE_FEED = 10;
const DELETE = 127;

/**
 * Where the sequence starting at `at` ends, so the caller can skip it.
 *
 * `at` is the escape itself. Every branch answers past the sequence's last
 * character. An unterminated OSC answers the end of the text, and an
 * unterminated CSI answers the first byte that cannot be part of one - which
 * is friendlier than a terminal, and leaves the text after a truncated
 * sequence rather than eating it.
 */
function endOfSequence(text: string, at: number): number {
  const next = text.charCodeAt(at + 1);
  if (Number.isNaN(next)) return text.length;

  // A STRING CONTROL carries a payload: OSC `]` (a window title, a hyperlink, a
  // notification), DCS `P`, SOS `X`, PM `^` and APC `_`. Five introducers, and
  // the payload is never text.
  //
  // The four beyond OSC are what a device answers with - a terminal capability
  // query, a graphics payload - and reading them as the escape and one
  // character puts `q~xyz` on the page where a terminal drew nothing.
  //
  // **One arm serves all five, and that is a deliberate simplification.**
  // Strictly, only OSC ends at a bell; the other four end at a string
  // terminator alone (ECMA-48, and xterm agrees). So a bell inside a DCS
  // payload cuts it there and its tail prints as text - narrow, and the same
  // simplification the terminal's own copy carries, which is the property
  // worth more here than the four bytes: two strippers that disagree about
  // what to strip are two pages that draw the same conversation differently.
  if (next === 0x5d || next === 0x50 || next === 0x58 || next === 0x5e || next === 0x5f) {
    for (let scan = at + 2; scan < text.length; scan += 1) {
      const code = text.charCodeAt(scan);
      if (code === BELL) return scan + 1;
      if (code === ESCAPE && text.charCodeAt(scan + 1) === 0x5c) return scan + 2;
    }
    return text.length;
  }

  // CSI ends at a byte in `@` to `~` once the parameter and intermediate
  // bytes are behind it.
  if (next === 0x5b) {
    let scan = at + 2;
    while (scan < text.length) {
      const code = text.charCodeAt(scan);
      const final = code >= 0x40 && code <= 0x7e;
      const parameter = code >= 0x30 && code <= 0x3f;
      const intermediate = code >= 0x20 && code <= 0x2f;
      if (final) return scan + 1;
      if (!parameter && !intermediate) return scan;
      scan += 1;
    }
    return text.length;
  }

  // An escape with an INTERMEDIATE byte is three long, and this is the family
  // a reset is written with: `sgr0` on this machine's terminfo is `\E(B\E[m`,
  // so reading `ESC ( B` as two bytes leaves a bare `B` on the page where a
  // terminal drew nothing.
  if (next >= 0x20 && next <= 0x2f) {
    const last = text.charCodeAt(at + 2);
    return Number.isNaN(last) ? text.length : at + 3;
  }

  // Every other escape is the escape and one character.
  return at + 2;
}

/**
 * `text` with the sequences a terminal would have obeyed taken out.
 *
 * Tab and newline survive, because they are layout the writer meant.
 * Carriage return does not: it is an instruction to overwrite a line the page
 * is not drawing line by line, so obeying it would need a terminal and
 * printing it would leave a control character in the text.
 */
export function stripEscapes(text: string): string {
  let out = '';
  let at = 0;
  while (at < text.length) {
    const code = text.charCodeAt(at);
    if (code === ESCAPE) {
      at = endOfSequence(text, at);
      continue;
    }
    // Anything below a space is a control the terminal acts on, save the two
    // that are layout.
    if (code < 0x20 ? code === TAB || code === LINE_FEED : code !== DELETE) out += text[at];
    at += 1;
  }
  return out;
}

/**
 * The first line of a body that says anything.
 *
 * A card's row is one line by the design's own rule and a message can be any
 * length, so what goes on the row is its first real line. The full text stays
 * where it belongs, in the body the row opens on.
 */
export function firstLine(text: string): string {
  return (
    text
      .split('\n')
      .find((line) => line.trim() !== '')
      ?.trim() ?? ''
  );
}

/**
 * A body as one line, ready for inline markdown: block marks off, lines joined,
 * space collapsed.
 *
 * **For a thought, the first line is often a stub** - "Let me orient. State:" -
 * with the substance a line or two below, so a row previewing `firstLine` says
 * nothing. The row shows the text whole and the layout breaks it where it runs
 * out. Block marks go because one line cannot draw a heading, a list or a
 * fence: they would land as raw `##` and `- ` where the words should be. Inline
 * marks - emphasis, code - stay, and the row renders them the way the body
 * does.
 */
export function joinedLine(text: string): string {
  return text
    .replace(/^```.*$/gm, '')
    .replace(/^#{1,6}\s+/gm, '')
    .replace(/^>\s?/gm, '')
    .replace(/^\s*(?:[-*+]|\d+[.)])\s+/gm, '')
    .replace(/!?\[([^\]]*)\]\([^)]*\)/g, '$1')
    .replace(/\s+/g, ' ')
    .trim();
}

/** A body's paragraphs, which are the blank-line breaks it arrives with. */
export function paragraphs(body: string): string[] {
  return body
    .split('\n\n')
    .map((one) => one.trim())
    .filter((one) => one !== '');
}

/**
 * What names a call on its own row.
 *
 * Read from the call's own input, in the order the kind of call makes sense:
 * a read names its file, a command names what it was for, a search names what
 * it looked for, a fetch names where, a skill names which skill it invoked.
 * Anything else keeps the tool's own name, which is honest about a row this
 * page has no better word for.
 *
 * The order matters because a call carries several of these keys: an edit has
 * both a path and a diff, and a command has both its own words and the
 * description it was given. Taking the first key present rather than the first
 * one that applies is how a command ends up named by its own text when the
 * description was the thing a reader needed.
 */
export function headline(name: string, input: unknown): string {
  const fields = (input ?? {}) as Record<string, unknown>;
  const text = (key: string): string | null => {
    const value = fields[key];
    return typeof value === 'string' && value.trim() !== '' ? value : null;
  };

  if (name === 'Bash' || name === 'BashOutput') {
    return text('description') ?? text('command') ?? name;
  }
  if (name === 'Skill') {
    // The skill's own name, which is the argument the call carries and the
    // thing the family label cannot say: `skill` names the class, and the
    // tool's own name is `Skill`, so the row would otherwise repeat itself.
    // `advisor` is not here: it carries `query`, and the chain below already
    // names it.
    const skill = text('skill');
    if (skill === null) return name;
    const args = text('args');
    return args === null ? skill : `${skill} ${args}`;
  }
  return (
    text('file_path') ??
    // Before `path`, because the calls that carry both - a search's pattern
    // and the tree it searched - are named by what they looked FOR.
    text('pattern') ??
    text('path') ??
    text('query') ??
    text('url') ??
    text('command') ??
    text('description') ??
    name
  );
}

/**
 * A path without the working directory the reader is already in, which is what
 * the row draws: a conversation is one session in one tree, so the prefix is
 * the same on every row and says nothing.
 */
export function shortPath(path: string, cwd: string | null): string {
  if (cwd === null) return path;
  const cut = `${cwd.replace(/\/$/, '')}/`;
  return path.startsWith(cut) ? path.slice(cut.length) : path;
}

/** One hit of a search call: where it is, and the line the match sits in. */
export interface SearchHit {
  path: string;
  line: string;
  /** The line's own text, which the row draws under its location. */
  src: string;
}

/**
 * `path:line:content` as three parts, or `null` for a line that is not that
 * shape.
 */
function splitHit(line: string): SearchHit | null {
  const first = line.indexOf(':');
  if (first <= 0) return null;
  const path = line.slice(0, first);
  if (path.includes(' ')) return null;
  const rest = line.slice(first + 1);
  const second = rest.indexOf(':');
  const number = second === -1 ? rest : rest.slice(0, second);
  if (number === '' || !/^\d+$/.test(number)) return null;
  const src = second === -1 ? '' : rest.slice(second + 1).trimStart();
  return { path, line: number, src };
}

/**
 * What a search call came back with, one hit per row, or `null` when the text
 * is not a set of hits.
 *
 * The wire's shape is `path:line:content`, and every line has to be one: a
 * single line that is not is the whole body reading as something else - a
 * command's output, a file's contents - and drawing it as hits would put half
 * a log behind line numbers.
 *
 * **The location and the line are two elems, not one string with a newline in
 * it.** The design drew them as a single run with a line break character,
 * which works only where a rule has already made the box pre-formatted: here
 * nothing does, so the newline collapses and every hit draws as one line with
 * its own location and text run together.
 */
export function searchHits(text: string): SearchHit[] | null {
  const lines = text.split('\n').filter((line) => line.trim() !== '');
  if (lines.length === 0) return null;
  const hits: SearchHit[] = [];
  for (const line of lines) {
    const hit = splitHit(line);
    if (hit === null) return null;
    hits.push({ ...hit, src: stripEscapes(hit.src) });
  }
  return hits;
}

/**
 * A tool's own name, with the server it belongs to taken off.
 *
 * An MCP tool is named `mcp__<server>__<tool>` on the wire, and the row it
 * sits on already says the server: the family lane is named for it. Repeating
 * it in the title spends the row's width on a word the reader has just read.
 */
export function toolName(name: string): string {
  if (!name.startsWith('mcp__')) return name;
  const afterServer = name.slice('mcp__'.length);
  // Everything after the SECOND separator, not a split on both: a tool's own
  // name can carry a separator of its own, and `agents__list` is one word.
  const cut = afterServer.indexOf('__');
  const rest = cut === -1 ? afterServer : afterServer.slice(cut + 2);
  return rest === '' ? name : rest;
}
