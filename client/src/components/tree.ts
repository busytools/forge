/**
 * One tree's rows, in draw order: a row per directory and per file, folders
 * before files at every level and alphabetical within each, indented by
 * depth. A directory row names its basename and carries every file beneath
 * it - what a collapsed row is hiding. A closed directory keeps its row and
 * no subtree.
 *
 * Lifted from the review page's own derivation, generic over what a file row
 * carries and how an item spells its path, so the review page's tree and the
 * files page's tree are ONE derivation rather than two.
 */

export interface TreeRow<T> {
  depth: number;
  kind: 'dir' | 'file';
  name: string;
  path: string;
  /** Files beneath, on a directory row; null on a file. */
  count: number | null;
  /** The item, on a file row; null on a directory. */
  file: T | null;
}

interface TreeNode<T> {
  dirs: Map<string, TreeNode<T>>;
  files: T[];
}

/** Add-or-remove `key` in a closed set: the tree's own toggling, shared by
 *  the pages that draw one. */
export function toggleClosed(closed: Set<string>, key: string): void {
  if (closed.has(key)) closed.delete(key);
  else closed.add(key);
}

export function treeRowsOf<T>(
  items: T[],
  closed: ReadonlySet<string>,
  pathOf: (item: T) => string,
): TreeRow<T>[] {
  const root: TreeNode<T> = { dirs: new Map(), files: [] };
  const base = (path: string): string => path.slice(path.lastIndexOf('/') + 1);
  for (const item of items) {
    const parts = pathOf(item).split('/');
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
    at.files.push(item);
  }
  const byName = (a: string, b: string): number => (a < b ? -1 : a > b ? 1 : 0);
  const countOf = (node: TreeNode<T>): number => {
    let total = node.files.length;
    for (const child of node.dirs.values()) total += countOf(child);
    return total;
  };
  const rows: TreeRow<T>[] = [];
  const descend = (node: TreeNode<T>, depth: number, prefix: string): void => {
    for (const [name, child] of [...node.dirs.entries()].sort(([a], [b]) => byName(a, b))) {
      const path = prefix === '' ? name : `${prefix}/${name}`;
      rows.push({ depth, kind: 'dir', name, path, count: countOf(child), file: null });
      if (!closed.has(path)) descend(child, depth + 1, path);
    }
    for (const file of [...node.files].sort((a, b) => byName(base(pathOf(a)), base(pathOf(b))))) {
      rows.push({
        depth,
        kind: 'file',
        name: base(pathOf(file)),
        path: pathOf(file),
        count: null,
        file,
      });
    }
  };
  descend(root, 0, '');
  return rows;
}
