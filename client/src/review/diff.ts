/**
 * What the review canvas derives from the record's diff: the file list of
 * one scope, the file tree, the counts, the unified text the viewer parses,
 * and the lookup behind the composer. Pure - the component draws what these
 * return.
 */

import { treeRowsOf, type TreeRow } from '../components/tree';
import type { DiffFileRecord, DiffHunkRecord, DiffLayerRecord } from '../session/wire';

/** One scope's files, one entry per populated layer; nothing for a clean or
 *  unreadable one, which the component draws as its own state instead. */
export function filesOf(layer: DiffLayerRecord): DiffFileRecord[] {
  return layer.state === 'populated' ? layer.files : [];
}

/** One file's counts across its hunks, what a row states beside it. */
export function lineCounts(file: DiffFileRecord): { plus: number; minus: number } {
  let plus = 0;
  let minus = 0;
  for (const hunk of file.hunks) {
    for (const line of hunk.lines) {
      if (line.kind === 'added') plus += 1;
      else if (line.kind === 'removed') minus += 1;
    }
  }
  return { plus, minus };
}

/**
 * The scope's files as their own tree, through the shared derivation
 * (`components/tree.ts`) both this page and the files page draw from - one
 * tree, not two.
 */
export function treeRows(
  files: DiffFileRecord[],
  closed: ReadonlySet<string>,
): TreeRow<DiffFileRecord>[] {
  return treeRowsOf(files, closed, (file) => file.path);
}

/** One file's whole content, as the read answered it. */
export interface FileLines {
  lines: string[];
  truncated: boolean;
}

/**
 * One gap the drawn hunks leave: the lines between two hunks, above the
 * first or below the last, that the record's bounded content does not
 * carry.
 */
export interface Gap {
  /** The hunk the gap follows; -1 is the gap above the first hunk. */
  after: number;
  /** The first new-side line the gap hides. */
  start: number;
  /** How many lines it hides. */
  count: number;
}

/**
 * The gaps a file's hunks leave, as far as the read in hand can fill them.
 *
 * **A gap is offered exactly where the read can supply it**, so a file
 * nothing has read - or one whose read refused - offers none, and a door
 * that would open on a failed read is never drawn. A read cut at the
 * ceiling offers the gaps its carried lines reach; the gap below the last
 * hunk exists only in a complete read, because a cut one cannot say where
 * the file ends - and a file whose hunks the record itself cut (`truncated`)
 * offers none either: the hunk after the cut is missing, so the lines the
 * gap would draw as context may be code the diff changed.
 */
export function gapsOf(file: DiffFileRecord, content: FileLines | null): Gap[] {
  if (content === null || file.truncated) return [];
  const hunks = file.hunks;
  const first = hunks[0];
  if (first === undefined) return [];
  const carried = content.lines.length;
  const gaps: Gap[] = [];
  if (first.newStart > 1) gaps.push({ after: -1, start: 1, count: first.newStart - 1 });
  for (let i = 0; i + 1 < hunks.length; i += 1) {
    const held = hunks[i];
    const next = hunks[i + 1];
    if (held === undefined || next === undefined) continue;
    const start = held.newStart + held.newCount;
    const count = next.newStart - start;
    if (count > 0) gaps.push({ after: i, start, count });
  }
  const last = hunks[hunks.length - 1];
  if (last !== undefined && !content.truncated) {
    const start = last.newStart + last.newCount;
    const count = carried - start + 1;
    if (count > 0) gaps.push({ after: hunks.length - 1, start, count });
  }
  return gaps.filter((gap) => gap.start + gap.count - 1 <= carried);
}

/**
 * One gap's reveal: how many of its hidden lines each end has opened. The
 * two ends are the gap's two controls - `down` the lines the hunk it follows
 * left off at, `up` the lines just above the hunk after it - so a press adds
 * to one end without touching the other.
 */
export interface GapReveal {
  down: number;
  up: number;
}

/** Lines one directional press reveals (GitHub's own step). */
export const GAP_STEP = 20;

/** At or below this many remaining lines, one press opens the rest and the
 *  control reads "expand all" rather than a direction - GitHub's own rule,
 *  taken from its page: a gap that small fits one fetch. */
export const GAP_SMALL = 20;

/** The hidden lines a gap still holds, given what the reader opened. */
export function gapRemaining(gap: Gap, reveal: GapReveal | undefined): number {
  const opened = (reveal?.down ?? 0) + (reveal?.up ?? 0);
  return Math.max(0, gap.count - opened);
}

/** What an expansion draws: the file's whole carried lines, and what the
 *  reader opened per gap, by the hunk each gap follows. */
export interface Expansion {
  content: FileLines;
  expanded: ReadonlyMap<number, GapReveal>;
}

/**
 * The wire's bounded hunks re-emitted as one unified diff text per file.
 *
 * The viewer parses unified diff text and nothing else, and the record
 * carries hunks rather than file contents - so the text is rebuilt here
 * (headers from the two ranges, one marker per line) rather than fetched.
 * The rebuild is lossless for display: it copies what the crate's parser
 * kept, and the crate's parser kept everything the text carried.
 *
 * **A revealed gap is drawn as context lines around the hunks it sits
 * between** - the same lines the diff left out, carrying the markers a
 * context line does - so the viewer computes their two side's numbers from
 * the heading exactly as it does every other line's. Each heading takes the
 * added lines into its own counts: a block that grew by them but kept its
 * old count would number every hunk below it wrong.
 *
 * **A gap the reader has fully revealed joins the two hunks into one
 * block**: the second heading goes, and the lines run continuously the way
 * GitHub draws a closed gap.
 */
export function synthesizeFileDiff(
  file: DiffFileRecord,
  expansion: Expansion | null = null,
): string {
  const out = [
    `diff --git a/${file.oldPath ?? file.path} b/${file.path}`,
    `--- a/${file.oldPath ?? file.path}`,
    `+++ b/${file.path}`,
  ];
  const hunks = file.hunks;
  const gaps = expansion === null ? [] : gapsOf(file, expansion.content);
  const revealOf = (after: number): GapReveal =>
    expansion?.expanded.get(after) ?? { down: 0, up: 0 };
  const gapAfter = (after: number): Gap | undefined => gaps.find((held) => held.after === after);
  /** A run of the gap's own lines, from `at` on, as context rows. */
  const runOf = (at: number, count: number): string[] =>
    expansion === null
      ? []
      : expansion.content.lines.slice(at - 1, at - 1 + count).map((text) => ` ${text}`);
  const rowOf = (line: DiffHunkRecord['lines'][number]): string =>
    (line.kind === 'added' ? '+' : line.kind === 'removed' ? '-' : ' ') + line.text;
  let i = 0;
  while (i < hunks.length) {
    const hunk = hunks[i];
    if (hunk === undefined) break;
    let oldStart = hunk.oldStart;
    let newStart = hunk.newStart;
    let oldCount = hunk.oldCount;
    let newCount = hunk.newCount;
    const body: string[] = [];
    // The bottom of the gap above this block takes the block's own heading:
    // the lines sit before its first line, so both ranges start earlier.
    const above = gapAfter(i === 0 ? -1 : i - 1);
    if (above !== undefined) {
      const head = runOf(
        above.start + above.count - revealOf(above.after).up,
        revealOf(above.after).up,
      );
      body.push(...head);
      oldStart -= head.length;
      newStart -= head.length;
      oldCount += head.length;
      newCount += head.length;
    }
    body.push(...hunk.lines.map(rowOf));
    // Gaps below this block: the tail the reader opened, and the following
    // hunk itself once the whole gap is open.
    let j = i;
    for (;;) {
      const gap = gapAfter(j);
      if (gap === undefined) break;
      const reveal = revealOf(gap.after);
      const remaining = gapRemaining(gap, reveal);
      if (reveal.down + reveal.up === 0) break;
      if (remaining > 0) {
        const tail = runOf(gap.start, reveal.down);
        body.push(...tail);
        oldCount += tail.length;
        newCount += tail.length;
        break;
      }
      const all = runOf(gap.start, gap.count);
      body.push(...all);
      oldCount += all.length;
      newCount += all.length;
      const next = hunks[j + 1];
      if (next === undefined) break;
      body.push(...next.lines.map(rowOf));
      oldCount += next.oldCount;
      newCount += next.newCount;
      j += 1;
    }
    out.push(`@@ -${oldStart},${oldCount} +${newStart},${newCount} @@`, ...body);
    i = j + 1;
  }
  return out.join('\n') + '\n';
}

/**
 * The hunk and line index a `(side, line)` names - what the composer needs
 * to know which line a click landed on. The naming is unique per side
 * within a file, so the first match is the only match.
 */
export function findLine(
  file: DiffFileRecord,
  side: 'old' | 'new',
  line: number,
): { hunk: DiffHunkRecord; index: number } | null {
  for (const hunk of file.hunks) {
    for (let index = 0; index < hunk.lines.length; index += 1) {
      const held = hunk.lines[index];
      if (held === undefined) continue;
      const at = side === 'old' ? held.oldLine : held.newLine;
      if (at === line) return { hunk, index };
    }
  }
  return null;
}

/** One line row's own side and line, or nothing for a row with neither. */
interface RowSide {
  side: 'old' | 'new';
  line: number;
}

/** One drawn file's own box, as the reading edge reads the canvas. */
interface SectionBox {
  path: string;
  top: number;
  bottom: number;
}

/**
 * The file the reader is inside: the drawn section whose region holds the
 * reading edge - the last one whose top has passed it - or `null` when the
 * edge sits above every drawn section.
 *
 * The boxes are the sections the canvas has actually drawn (the list is
 * virtualised, so a section the reader has scrolled past is gone, and the
 * one at the edge is by definition among those drawn). Order-independent:
 * the box with the greatest top at or above the edge wins.
 */
export function currentFileAt(sections: SectionBox[], edge: number): string | null {
  let best: SectionBox | null = null;
  for (const section of sections) {
    if (section.top > edge + 1) continue;
    if (best === null || section.top > best.top) best = section;
  }
  return best === null ? null : best.path;
}

/**
 * The viewer's own DOM, read back: the line numbers and cell geometry its
 * two modes draw, as the page's own decisions about them.
 *
 * These are the shapes a live page renders, captured from it rather than
 * guessed: unified rows carry one number cell holding BOTH numbers (so a
 * context line has two), split rows carry a cell per side, and a split row
 * pads its missing half with one spanning placeholder cell - so rows in one
 * file do not all carry the same number of cells. They are pure functions
 * over elements so a test can hand them tables of exactly those shapes:
 * mounting the viewer itself under jsdom is not possible (its text
 * measurement allocates without bound there).
 */

/** The line a number span names, whichever attribute form carries it. */
export function lineOf(span: HTMLElement): number | null {
  const line = Number(span.dataset['lineNewNum'] ?? span.dataset['lineOldNum'] ?? span.textContent);
  return Number.isInteger(line) && line > 0 ? line : null;
}

/**
 * The side and line a line-row click means: the clicked cell's own side in
 * split, and in unified the new number where the row has one - a removed
 * line has only the old.
 */
export function rowSide(
  row: Element,
  target: HTMLElement | null,
  mode: 'unified' | 'split',
): RowSide | null {
  const numOf = (side: 'old' | 'new'): number | null => {
    const selector =
      mode === 'unified'
        ? `span[data-line-${side}-num]`
        : `td[data-side="${side}"] span[data-line-num], tr[data-side="${side}"] span[data-line-num]`;
    const span = row.querySelector(selector);
    return span instanceof HTMLElement ? lineOf(span) : null;
  };
  const at = (side: 'old' | 'new'): RowSide | null => {
    const line = numOf(side);
    return line === null ? null : { side, line };
  };
  const clicked =
    mode === 'split' ? target?.closest('td[data-side]')?.getAttribute('data-side') : null;
  if (clicked === 'old') return at('old') ?? at('new');
  return at('new') ?? at('old');
}

/** The row a line's number sits on, by the side that number belongs to. */
export function anchorRow(
  el: HTMLElement,
  side: 'old' | 'new',
  line: number,
  mode: 'unified' | 'split',
): HTMLTableRowElement | null {
  const anchor =
    mode === 'unified'
      ? el.querySelector(`span[data-line-${side}-num="${line}"]`)
      : (el.querySelector(`td[data-side="${side}"] span[data-line-num="${line}"]`) ??
        el.querySelector(`tr[data-side="${side}"] span[data-line-num="${line}"]`));
  return anchor?.closest('tr') ?? null;
}

/**
 * The cell one SPLIT line row's door belongs in, and the line it comments
 * on. Unified has no per-side cell: its door is one per row, found through
 * [`rowSide`].
 */
export function doorSpot(
  row: Element,
  side: 'old' | 'new',
  mode: 'unified' | 'split',
): { cell: HTMLElement; line: number } | null {
  if (mode !== 'split') return null;
  const selector = `td[data-side="${side}"] span[data-line-num], tr[data-side="${side}"] span[data-line-num]`;
  const span = row.querySelector(selector);
  if (!(span instanceof HTMLElement)) return null;
  const cell = span.closest('td') ?? span.parentElement;
  const line = lineOf(span);
  if (!(cell instanceof HTMLElement) || line === null) return null;
  return { cell, line };
}

/**
 * One inserted row, shaped after the row it lands under. **In split the
 * card holds the half it comments on** - a right-side comment under a
 * right-side line, a left-side one under the left - and in unified it takes
 * the whole lane, where the line itself does.
 */
export function insertRow(
  at: HTMLTableRowElement,
  side: 'old' | 'new',
  mode: 'unified' | 'split',
  body: HTMLElement,
): HTMLTableRowElement {
  const row = document.createElement('tr');
  row.className = 'rv-insert';
  const cells = [...at.children].map((cell) => {
    const el = cell as HTMLElement;
    const own =
      el.dataset['side'] ??
      (el.className.includes('old') ? 'old' : el.className.includes('new') ? 'new' : null);
    return { side: own, colspan: Number(el.getAttribute('colspan') ?? 1) };
  });
  const card = (colspan: number): HTMLTableCellElement => {
    const cell = document.createElement('td');
    cell.className = 'rv-insert-cell';
    cell.colSpan = colspan;
    return cell;
  };
  const spanOf = (which: 'old' | 'new'): number =>
    cells.filter((cell) => cell.side === which).reduce((n, cell) => n + cell.colspan, 0);
  // No cell names a side: unified, whose line owns the whole width.
  if (mode === 'unified' || !cells.some((cell) => cell.side !== null)) {
    const cell = card(cells.reduce((n, held) => n + held.colspan, 0));
    cell.append(body);
    row.append(cell);
    return row;
  }
  const mine = card(spanOf(side));
  mine.append(body);
  const theirs = card(spanOf(side === 'new' ? 'old' : 'new'));
  if (side === 'new') row.append(theirs, mine);
  else row.append(mine, theirs);
  return row;
}
