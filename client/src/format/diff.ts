/**
 * Word-level ranges inside a paired removed/added line.
 *
 * **Pairing is line-by-line within a hunk**: the nth removed line pairs with
 * the nth added line after the preceding context, and an unpaired line gets
 * the row tint only. The ranges are character offsets into the line, so a
 * surface can tint the changed words without losing the grammar's own
 * colours - the tint wraps a slice inside a token, it never replaces it.
 */

/** One line of a change, as the pairing reads it. */
export interface SourceLine {
  kind: 'ctx' | 'del' | 'add';
  text: string;
}

/** A changed run of characters. */
export interface Span {
  start: number;
  end: number;
}

/** A word-diff operation over two paired lines, in reading order. */
export interface DiffOp {
  kind: 'equal' | 'del' | 'ins';
  text: string;
}

/**
 * Two sides of an edit, as a line diff.
 *
 * **A mutation's own input carries the replaced text, not a hunk**, so the
 * CLI's empty `structuredPatch` fallback used to draw every old line as a
 * removal and every new one as an addition even when they shared all but a
 * word. This is the line-level read the fallback and a hunk may both use:
 * common prefix and suffix become context, and the middle is walked for the
 * longest common subsequence so an unchanged line between two changes stays
 * context too.
 *
 * Past the cap the middle is drawn as one removed run and one added run:
 * the quadratic table is the reader's cost, and an edit block large enough
 * to need it is a replacement rather than a line edit.
 */
export function lineDiff(oldText: string, newText: string): SourceLine[] {
  const oldLines = splitLines(oldText);
  const newLines = splitLines(newText);

  let prefix = 0;
  while (
    prefix < oldLines.length &&
    prefix < newLines.length &&
    oldLines[prefix] === newLines[prefix]
  ) {
    prefix += 1;
  }
  let suffix = 0;
  while (
    suffix < oldLines.length - prefix &&
    suffix < newLines.length - prefix &&
    oldLines[oldLines.length - 1 - suffix] === newLines[newLines.length - 1 - suffix]
  ) {
    suffix += 1;
  }

  const oldMid = oldLines.slice(prefix, oldLines.length - suffix);
  const newMid = newLines.slice(prefix, newLines.length - suffix);
  const out: SourceLine[] = [];
  for (const text of oldLines.slice(0, prefix)) out.push({ kind: 'ctx', text });

  const cells = oldMid.length * newMid.length;
  if (cells === 0 || cells > MAX_LINE_CELLS) {
    for (const text of oldMid) out.push({ kind: 'del', text });
    for (const text of newMid) out.push({ kind: 'add', text });
  } else {
    // The table is filled from the end, so the forward walk can read the
    // next cell's score without carrying a path of its own.
    const table: number[][] = Array.from({ length: oldMid.length + 1 }, () =>
      new Array<number>(newMid.length + 1).fill(0),
    );
    for (let a = oldMid.length - 1; a >= 0; a -= 1) {
      const row = table[a];
      const below = table[a + 1];
      if (row === undefined || below === undefined) continue;
      for (let b = newMid.length - 1; b >= 0; b -= 1) {
        row[b] =
          oldMid[a] === newMid[b]
            ? (below[b + 1] ?? 0) + 1
            : Math.max(below[b] ?? 0, row[b + 1] ?? 0);
      }
    }
    let a = 0;
    let b = 0;
    while (a < oldMid.length || b < newMid.length) {
      if (a < oldMid.length && b < newMid.length && oldMid[a] === newMid[b]) {
        out.push({ kind: 'ctx', text: oldMid[a] ?? '' });
        a += 1;
        b += 1;
        continue;
      }
      const down = table[a + 1]?.[b] ?? 0;
      const right = table[a]?.[b + 1] ?? 0;
      if (b < newMid.length && (a >= oldMid.length || right > down)) {
        out.push({ kind: 'add', text: newMid[b] ?? '' });
        b += 1;
      } else {
        out.push({ kind: 'del', text: oldMid[a] ?? '' });
        a += 1;
      }
    }
  }

  for (const text of oldLines.slice(oldLines.length - suffix)) out.push({ kind: 'ctx', text });
  return out;
}

/** A trailing newline ends the last line rather than starting an empty one. */
function splitLines(text: string): string[] {
  if (text === '') return [];
  const lines = text.split('\n');
  if (lines[lines.length - 1] === '') lines.pop();
  return lines;
}

/** Past this many line pairs the fallback draws the middle as one run. */
const MAX_LINE_CELLS = 250_000;

interface Word {
  text: string;
  start: number;
  end: number;
}

/** Words, runs of space, and single punctuation: the units a reader sees change. */
const WORD = /[A-Za-z0-9_]+|\s+|[^\sA-Za-z0-9_]/g;

/**
 * Past this many words a side is not word-diffed at all: the quadratic walk
 * is the renderer's cost, and a diff line can be hundreds of thousands of
 * characters (one real record carried 441,094). The row tint still draws.
 */
const MAX_WORDS = 400;

function words(text: string): Word[] {
  const out: Word[] = [];
  for (const match of text.matchAll(WORD)) {
    const at = match.index ?? 0;
    out.push({ text: match[0], start: at, end: at + match[0].length });
  }
  return out;
}

/** The words both sides share before and after the change. */
function common(prefixOf: Word[], suffixOf: Word[]): { prefix: number; suffix: number } {
  let prefix = 0;
  while (
    prefix < prefixOf.length &&
    prefix < suffixOf.length &&
    prefixOf[prefix]?.text === suffixOf[prefix]?.text
  ) {
    prefix += 1;
  }
  let suffix = 0;
  while (
    suffix < prefixOf.length - prefix &&
    suffix < suffixOf.length - prefix &&
    prefixOf[prefixOf.length - 1 - suffix]?.text === suffixOf[suffixOf.length - 1 - suffix]?.text
  ) {
    suffix += 1;
  }
  return { prefix, suffix };
}

/** Which words of each side are outside the longest common subsequence. */
function changedIn(a: Word[], b: Word[]): { a: boolean[]; b: boolean[] } {
  const n = a.length;
  const m = b.length;
  const table: number[][] = Array.from({ length: n + 1 }, () => new Array<number>(m + 1).fill(0));
  for (let i = n - 1; i >= 0; i -= 1) {
    for (let j = m - 1; j >= 0; j -= 1) {
      const row = table[i];
      const next = table[i + 1];
      if (row === undefined || next === undefined) continue;
      row[j] =
        a[i]?.text === b[j]?.text
          ? (next[j + 1] ?? 0) + 1
          : Math.max(next[j] ?? 0, row[j + 1] ?? 0);
    }
  }
  const changedA = new Array<boolean>(n).fill(true);
  const changedB = new Array<boolean>(m).fill(true);
  let i = 0;
  let j = 0;
  while (i < n && j < m) {
    if (a[i]?.text === b[j]?.text) {
      changedA[i] = false;
      changedB[j] = false;
      i += 1;
      j += 1;
    } else if ((table[i + 1]?.[j] ?? 0) >= (table[i]?.[j + 1] ?? 0)) {
      i += 1;
    } else {
      j += 1;
    }
  }
  return { a: changedA, b: changedB };
}

/**
 * The operations that turn `oldText` into `newText`, in reading order.
 *
 * A line whose middle is too long to diff answers one delete and one insert,
 * which draws the whole middle tinted rather than spending the renderer.
 */
export function wordOps(oldText: string, newText: string): DiffOp[] {
  const a = words(oldText);
  const b = words(newText);
  const { prefix, suffix } = common(a, b);
  const aMiddle = a.slice(prefix, a.length - suffix);
  const bMiddle = b.slice(prefix, b.length - suffix);

  const ops: DiffOp[] = [];
  const equalRun = (list: Word[], from: number, to: number): void => {
    const text = list
      .slice(from, to)
      .map((word) => word.text)
      .join('');
    if (text !== '') ops.push({ kind: 'equal', text });
  };
  equalRun(a, 0, prefix);

  if (aMiddle.length > MAX_WORDS || bMiddle.length > MAX_WORDS) {
    const removed = aMiddle.map((word) => word.text).join('');
    const added = bMiddle.map((word) => word.text).join('');
    if (removed !== '') ops.push({ kind: 'del', text: removed });
    if (added !== '') ops.push({ kind: 'ins', text: added });
  } else {
    const changed = changedIn(aMiddle, bMiddle);
    let i = 0;
    let j = 0;
    while (i < aMiddle.length || j < bMiddle.length) {
      if (i < aMiddle.length && j < bMiddle.length && !changed.a[i] && !changed.b[j]) {
        const run = aMiddle[i]?.text ?? '';
        if (ops.at(-1)?.kind === 'equal') {
          const last = ops.at(-1);
          if (last !== undefined) last.text += run;
        } else {
          ops.push({ kind: 'equal', text: run });
        }
        i += 1;
        j += 1;
        continue;
      }
      let removed = '';
      while (i < aMiddle.length && changed.a[i] === true) {
        removed += aMiddle[i]?.text ?? '';
        i += 1;
      }
      let added = '';
      while (j < bMiddle.length && changed.b[j] === true) {
        added += bMiddle[j]?.text ?? '';
        j += 1;
      }
      if (removed !== '') ops.push({ kind: 'del', text: removed });
      if (added !== '') ops.push({ kind: 'ins', text: added });
    }
  }

  equalRun(a, a.length - suffix, a.length);
  return ops;
}

/**
 * The changed character spans of one side, from the operations.
 *
 * **A run broken only by space or punctuation is one span.** The words on
 * either side changed, so a tint that stops at each word leaves the space and
 * the punctuation between them untinted - a highlight with holes. A shared
 * word between two changes is a different thing and keeps them apart.
 */
function spansOf(ops: DiffOp[], side: 'old' | 'new', text: string): Span[] {
  const spans: Span[] = [];
  let at = 0;
  for (const op of ops) {
    if (op.kind === 'equal' || (side === 'old' ? op.kind === 'del' : op.kind === 'ins')) {
      if (op.kind !== 'equal') {
        const last = spans.at(-1);
        const gap = last === undefined ? '' : text.slice(last.end, at);
        if (last !== undefined && (last.end === at || /^[^A-Za-z0-9_]*$/.test(gap))) {
          last.end = at + op.text.length;
        } else spans.push({ start: at, end: at + op.text.length });
      }
      at += op.text.length;
    }
  }
  return spans;
}

/** The changed spans of a paired removed and added line. */
export function wordSpans(oldText: string, newText: string): { old: Span[]; new: Span[] } {
  const ops = wordOps(oldText, newText);
  return { old: spansOf(ops, 'old', oldText), new: spansOf(ops, 'new', newText) };
}

/**
 * The word tint per line of a hunk, aligned with `lines`.
 *
 * Walks each run of removed lines and the added lines immediately after it,
 * pairing by position. A context line, an unpaired removal and a surplus
 * addition all answer no spans.
 */
export function pairedTints(lines: readonly SourceLine[]): Span[][] {
  const tints: Span[][] = lines.map(() => []);
  let i = 0;
  while (i < lines.length) {
    if (lines[i]?.kind !== 'del') {
      i += 1;
      continue;
    }
    let removed = i;
    while (removed < lines.length && lines[removed]?.kind === 'del') removed += 1;
    let added = removed;
    while (added < lines.length && lines[added]?.kind === 'add') added += 1;
    const pairs = Math.min(removed - i, added - removed);
    for (let k = 0; k < pairs; k += 1) {
      const oldLine = lines[i + k];
      const newLine = lines[removed + k];
      if (oldLine === undefined || newLine === undefined) continue;
      const spans = wordSpans(oldLine.text, newLine.text);
      tints[i + k] = spans.old;
      tints[removed + k] = spans.new;
    }
    i = added;
  }
  return tints;
}

/** The fields a colouring token carries, as the renderer reads them. */
export interface ColourToken {
  content: string;
  color?: string;
  fontStyle?: number;
}

/** `text` escaped, so a file's own characters never become markup. */
function escaped(text: string): string {
  return text.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
}

/** A token's own styles: the theme colour and the slant bits it carries. */
function styleOf(token: ColourToken): string {
  const bits = token.fontStyle ?? 0;
  const styles = [
    token.color === undefined ? '' : `color:${token.color}`,
    (bits & 1) === 1 ? 'font-style:italic' : '',
    (bits & 2) === 2 ? 'font-weight:bold' : '',
    (bits & 4) === 4 ? 'text-decoration:underline' : '',
  ].filter((style) => style !== '');
  return styles.length === 0 ? '' : ` style="${styles.join(';')}"`;
}

/** The tinted and untinted runs of one token's slice. */
function slices(
  from: number,
  to: number,
  tints: readonly Span[],
): { start: number; end: number; tinted: boolean }[] {
  const out: { start: number; end: number; tinted: boolean }[] = [];
  let at = from;
  for (const span of tints) {
    if (span.end <= at || span.start >= to) continue;
    const start = Math.max(at, span.start);
    const end = Math.min(to, span.end);
    if (start > at) out.push({ start: at, end: start, tinted: false });
    out.push({ start, end, tinted: true });
    at = end;
  }
  if (at < to) out.push({ start: at, end: to, tinted: false });
  return out;
}

/**
 * One diff line's body, escaped, with the word tint wrapping a slice INSIDE
 * the grammar's own token spans: the colours stay, the tint sits on top.
 */
export function diffLineHtml(
  text: string,
  tints: readonly Span[],
  tokens: readonly ColourToken[] | null,
): string {
  if (tokens === null) {
    return slices(0, text.length, tints)
      .map((slice) =>
        slice.tinted
          ? `<span class="w">${escaped(text.slice(slice.start, slice.end))}</span>`
          : escaped(text.slice(slice.start, slice.end)),
      )
      .join('');
  }
  let at = 0;
  let html = '';
  for (const token of tokens) {
    const end = at + token.content.length;
    const slice = slices(at, end, tints);
    for (const part of slice) {
      const content = escaped(token.content.slice(part.start - at, part.end - at));
      html += `<span${styleOf(token)}>${
        part.tinted ? `<span class="w">${content}</span>` : content
      }</span>`;
    }
    at = end;
  }
  if (at < text.length) {
    html += slices(at, text.length, tints)
      .map((part) =>
        part.tinted
          ? `<span class="w">${escaped(text.slice(part.start, part.end))}</span>`
          : escaped(text.slice(part.start, part.end)),
      )
      .join('');
  }
  return html;
}
