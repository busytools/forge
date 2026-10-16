/**
 * What the review canvas derives from the record's diff: the file list of
 * one scope, the file tree, the counts, the unified text the viewer parses,
 * and the lookup behind the composer. Pure - the component draws what these
 * return.
 */

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

/** One row of the tree in draw order: a directory, then what is beneath it,
 *  with a closed directory keeping its row and no subtree. The pairs are
 *  nullable rather than a discriminated union because the template reads
 *  them through the lint's type resolution, which does not narrow a union
 *  across an if. */
interface TreeRow {
  depth: number;
  kind: 'dir' | 'file';
  name: string;
  path: string;
  /** Files beneath, on a directory row; null on a file. */
  count: number | null;
  /** The record, on a file row; null on a directory. */
  file: DiffFileRecord | null;
}

interface TreeNode {
  dirs: Map<string, TreeNode>;
  files: DiffFileRecord[];
}

/**
 * The scope's files as their own tree: a row per directory and per file,
 * folders before files at every level and alphabetical within each, indented
 * by depth. A directory row names its basename and carries every file
 * beneath it - what a collapsed row is hiding.
 */
export function treeRows(files: DiffFileRecord[], closed: ReadonlySet<string>): TreeRow[] {
  const root: TreeNode = { dirs: new Map(), files: [] };
  const base = (path: string): string => path.slice(path.lastIndexOf('/') + 1);
  for (const file of files) {
    const parts = file.path.split('/');
    parts.pop();
    let at = root;
    let prefix = '';
    for (const part of parts) {
      prefix = prefix === '' ? part : `${prefix}/${part}`;
      let next = at.dirs.get(part);
      if (next === undefined) {
        next = { dirs: new Map(), files: [] };
        at.dirs.set(part, next);
      }
      at = next;
    }
    at.files.push(file);
  }
  const byName = (a: string, b: string): number => (a < b ? -1 : a > b ? 1 : 0);
  const countOf = (node: TreeNode): number => {
    let total = node.files.length;
    for (const child of node.dirs.values()) total += countOf(child);
    return total;
  };
  const rows: TreeRow[] = [];
  const descend = (node: TreeNode, depth: number, prefix: string): void => {
    for (const [name, child] of [...node.dirs.entries()].sort(([a], [b]) => byName(a, b))) {
      const path = prefix === '' ? name : `${prefix}/${name}`;
      rows.push({ depth, kind: 'dir', name, path, count: countOf(child), file: null });
      if (!closed.has(path)) descend(child, depth + 1, path);
    }
    for (const file of [...node.files].sort((a, b) => byName(base(a.path), base(b.path)))) {
      rows.push({ depth, kind: 'file', name: base(file.path), path: file.path, count: null, file });
    }
  };
  descend(root, 0, '');
  return rows;
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

/** What an expansion draws: the file's whole carried lines, and which gaps
 *  the reader opened, by the hunk each follows. */
export interface Expansion {
  content: FileLines;
  expanded: ReadonlySet<number>;
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
 * **An opened gap is drawn as context lines inside the hunk it follows** -
 * the same lines the diff left out, carrying the markers a context line
 * does - so the viewer computes their two side's numbers from the heading
 * exactly as it does every other line's. The heading takes the added lines
 * into its own counts: a block that grew by them but kept its old count
 * would number every hunk below it wrong.
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
  const gaps = expansion === null ? [] : gapsOf(file, expansion.content);
  /** The gap's own lines, when the reader opened it; nothing otherwise. */
  const opened = (after: number): string[] => {
    if (expansion === null || !expansion.expanded.has(after)) return [];
    const gap = gaps.find((held) => held.after === after);
    if (gap === undefined) return [];
    return expansion.content.lines.slice(gap.start - 1, gap.start - 1 + gap.count);
  };
  file.hunks.forEach((hunk, i) => {
    let { oldStart, newStart, oldCount, newCount } = hunk;
    const body: string[] = [];
    // The gap above the first hunk takes that hunk's own heading: the lines
    // sit before its first line, so both ranges start earlier by them.
    const above = opened(-1);
    if (i === 0 && above.length > 0) {
      body.push(...above.map((text) => ` ${text}`));
      oldStart -= above.length;
      newStart -= above.length;
      oldCount += above.length;
      newCount += above.length;
    }
    for (const line of hunk.lines) {
      const marker = line.kind === 'added' ? '+' : line.kind === 'removed' ? '-' : ' ';
      body.push(marker + line.text);
    }
    // A gap after a hunk is that hunk's own tail: only the counts grow.
    const below = opened(i);
    if (below.length > 0) {
      body.push(...below.map((text) => ` ${text}`));
      oldCount += below.length;
      newCount += below.length;
    }
    out.push(`@@ -${oldStart},${oldCount} +${newStart},${newCount} @@`, ...body);
  });
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
