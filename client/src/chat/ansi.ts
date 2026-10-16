/**
 * A command's own colour, read off the escapes it wrote and drawn in the
 * page's theme.
 *
 * **A terminal's escapes are a rendering of text, and this is the page's
 * rendering of them.** The server hands the command's bytes over as written;
 * the box that draws them reads the SGR parameters (colour, bold, dim,
 * italic, underline) into runs the sheet styles, and drops every sequence it
 * cannot draw rather than printing it - a browser has no terminal to obey a
 * cursor move, so printing one would put its residue on the page.
 *
 * **The colours are the theme's, not the command's codes**: each of the
 * sixteen maps to a token the sheet already owns, so a coloured row belongs to
 * the same palette as the rest of the page. Extended colour (`38;5;n`,
 * `38;2;r;g;b`) is read as the sequence it is and leaves the current colour
 * alone - nothing is invented for it, and nothing leaks.
 */

/** What a terminal acts on rather than prints. */
const ESCAPE = 27;
const BELL = 7;
const TAB = 9;
const LINE_FEED = 10;
const DELETE = 127;

/** One run of text that draws in one style. */
export interface AnsiRun {
  text: string;
  /** The sheet's classes for the run, space-separated; empty for the box's own tone. */
  cls: string;
}

/** The sixteen the theme has a token for, indexed the way SGR numbers them. */
const COLOURS = ['black', 'red', 'green', 'yellow', 'blue', 'magenta', 'cyan', 'white'] as const;

/** The style a run draws with, carried across the text as the escape sets it. */
interface Style {
  colour: string | null;
  bright: boolean;
  bold: boolean;
  dim: boolean;
  italic: boolean;
  underline: boolean;
}

function defaultStyle(): Style {
  return { colour: null, bright: false, bold: false, dim: false, italic: false, underline: false };
}

/** The classes `style` draws with, in the one order the sheet's rules assume. */
function classesOf(style: Style): string {
  const out: string[] = [];
  if (style.colour !== null) out.push(`ansi-${style.bright ? 'bright-' : ''}${style.colour}`);
  if (style.bold) out.push('ansi-bold');
  if (style.dim) out.push('ansi-dim');
  if (style.italic) out.push('ansi-italic');
  if (style.underline) out.push('ansi-underline');
  return out.join(' ');
}

/**
 * Apply one SGR parameter list to `style`, mutating it.
 *
 * Unknown parameters are ignored on purpose: a code this page has no style for
 * changes nothing rather than guessing one. The extended-colour parameters are
 * the case that matters - they arrive as several numbers, and each is simply
 * not a style this build draws.
 */
function applySgr(style: Style, params: number[]): void {
  const list = params.length === 0 ? [0] : params;
  for (const param of list) {
    if (param === 0) Object.assign(style, defaultStyle());
    else if (param === 1) style.bold = true;
    else if (param === 2) style.dim = true;
    else if (param === 3) style.italic = true;
    else if (param === 4) style.underline = true;
    else if (param === 22) {
      style.bold = false;
      style.dim = false;
    } else if (param === 23) style.italic = false;
    else if (param === 24) style.underline = false;
    else if (param >= 30 && param <= 37) {
      style.colour = COLOURS[param - 30] ?? null;
      style.bright = false;
    } else if (param === 39) {
      style.colour = null;
      style.bright = false;
    } else if (param >= 90 && param <= 97) {
      style.colour = COLOURS[param - 90] ?? null;
      style.bright = true;
    } else if (param === 99) {
      style.colour = null;
      style.bright = false;
    }
  }
}

/**
 * Where the sequence starting at `at` (the escape itself) ends, and its SGR
 * parameters when it is one.
 *
 * The scan mirrors `text.ts`'s stripper - unterminated sequences answer the
 * end of the text, an unterminated CSI answers the first byte that cannot be
 * part of one - so the two read the same bytes the same way.
 */
function sequenceAt(text: string, at: number): { end: number; sgr: number[] | null } {
  const next = text.charCodeAt(at + 1);
  if (Number.isNaN(next)) return { end: text.length, sgr: null };

  // A string control carries a payload and is never text: OSC `]`, DCS `P`,
  // SOS `X`, PM `^`, APC `_`.
  if (next === 0x5d || next === 0x50 || next === 0x58 || next === 0x5e || next === 0x5f) {
    for (let scan = at + 2; scan < text.length; scan += 1) {
      const code = text.charCodeAt(scan);
      if (code === BELL) return { end: scan + 1, sgr: null };
      if (code === ESCAPE && text.charCodeAt(scan + 1) === 0x5c)
        return { end: scan + 2, sgr: null };
    }
    return { end: text.length, sgr: null };
  }

  // CSI: parameters and intermediates, then a final byte in `@` to `~`.
  if (next === 0x5b) {
    let scan = at + 2;
    while (scan < text.length) {
      const code = text.charCodeAt(scan);
      const final = code >= 0x40 && code <= 0x7e;
      const parameter = code >= 0x30 && code <= 0x3f;
      const intermediate = code >= 0x20 && code <= 0x2f;
      if (final) {
        const isSgr = code === 0x6d;
        const raw = text.slice(at + 2, scan);
        const sgr = isSgr
          ? raw === ''
            ? []
            : raw.split(';').map((part) => (part === '' ? 0 : Number(part)))
          : null;
        return { end: scan + 1, sgr };
      }
      if (!parameter && !intermediate) return { end: scan, sgr: null };
      scan += 1;
    }
    return { end: text.length, sgr: null };
  }

  // An escape with an intermediate byte is three long (`ESC ( B`).
  if (next >= 0x20 && next <= 0x2f) {
    const last = text.charCodeAt(at + 2);
    return { end: Number.isNaN(last) ? text.length : at + 3, sgr: null };
  }

  // Every other escape is the escape and one character.
  return { end: at + 2, sgr: null };
}

/**
 * `text` as the runs a box draws: escape sequences read into styles, controls
 * a browser cannot obey dropped, layout bytes kept.
 */
export function ansiRuns(text: string): AnsiRun[] {
  const runs: AnsiRun[] = [];
  const style = defaultStyle();
  let current = '';
  let currentCls: string | null = null;
  const push = (): void => {
    if (current !== '') runs.push({ text: current, cls: currentCls ?? '' });
    current = '';
    currentCls = null;
  };
  for (let at = 0; at < text.length;) {
    const code = text.charCodeAt(at);
    if (code === ESCAPE) {
      const sequence = sequenceAt(text, at);
      if (sequence.sgr !== null) {
        // Only a style that actually MOVES closes the run: a sequence that
        // leaves the look as it was is part of the same run, which is what
        // keeps a command's repeated colour codes from splitting the text.
        const before = classesOf(style);
        applySgr(style, sequence.sgr);
        if (classesOf(style) !== before) push();
      }
      at = sequence.end;
      continue;
    }
    // Tab and newline are layout the writer meant; every other control is an
    // instruction this page cannot obey, so it is dropped rather than drawn.
    if (code < 0x20 ? code === TAB || code === LINE_FEED : code !== DELETE) {
      const cls = classesOf(style);
      if (current !== '' && cls !== currentCls) push();
      currentCls = cls;
      current += text[at];
    }
    at += 1;
  }
  push();
  return runs;
}
