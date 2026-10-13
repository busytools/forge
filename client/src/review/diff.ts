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

/**
 * The wire's bounded hunks re-emitted as one unified diff text per file.
 *
 * The viewer parses unified diff text and nothing else, and the record
 * carries hunks rather than file contents - so the text is rebuilt here
 * (headers from the two ranges, one marker per line) rather than fetched.
 * The rebuild is lossless for display: it copies what the crate's parser
 * kept, and the crate's parser kept everything the text carried.
 */
export function synthesizeFileDiff(file: DiffFileRecord): string {
  const out = [
    `diff --git a/${file.oldPath ?? file.path} b/${file.path}`,
    `--- a/${file.oldPath ?? file.path}`,
    `+++ b/${file.path}`,
  ];
  for (const hunk of file.hunks) {
    out.push(`@@ -${hunk.oldStart},${hunk.oldCount} +${hunk.newStart},${hunk.newCount} @@`);
    for (const line of hunk.lines) {
      const marker = line.kind === 'added' ? '+' : line.kind === 'removed' ? '-' : ' ';
      out.push(marker + line.text);
    }
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
