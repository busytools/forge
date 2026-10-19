/**
 * The files page's tree: the seat's whole index as rows, and the filter it
 * narrows by.
 *
 * The hierarchy is the shared derivation (`components/tree.ts`) the review
 * page already draws from; the filter is the `@` list's own matching
 * (`composer/autocomplete.ts`), so one query means one thing on both
 * surfaces. Nothing here decides how any of it looks.
 */

import { fileTier } from '../composer/autocomplete';
import type { FileEntry } from '../composer/wire';
import { treeRowsOf, type TreeRow } from '../components/tree';

/** The seat's whole tree, from the record's own file index. */
export function indexTreeRows(
  files: FileEntry[],
  closed: ReadonlySet<string>,
): TreeRow<FileEntry>[] {
  return treeRowsOf(files, closed, (file) => file.relPath);
}

/**
 * The folders that start folded: one holding a file directly, because a
 * file makes it leaf-bearing. A folder holding only folders starts open, so
 * the tree's skeleton reads without a click and no file rows show.
 */
export function foldedByDefault(files: FileEntry[]): Set<string> {
  const dirs = new Set<string>();
  for (const file of files) {
    const cut = file.relPath.lastIndexOf('/');
    if (cut > 0) dirs.add(file.relPath.slice(0, cut));
  }
  return dirs;
}

/** No folder folded: what a filter shows, since its job is the matches. */
export const NO_FOLDS: ReadonlySet<string> = new Set<string>();

/**
 * The folders drawn shut: the defaults, with the reader's own folds over
 * them. An explicit fold wins either way, so a default-open folder can be
 * shut and a default-shut one opened.
 */
export function closedFolders(
  defaults: ReadonlySet<string>,
  folded: ReadonlyMap<string, boolean>,
): Set<string> {
  const shut = new Set<string>();
  for (const path of defaults) if (folded.get(path) !== false) shut.add(path);
  for (const [path, isShut] of folded) if (isShut && !defaults.has(path)) shut.add(path);
  return shut;
}

/**
 * The entries `query` keeps - by the `@` list's own tiers, so a query that
 * would find a file there finds it here. An empty query keeps everything;
 * the tree re-sorts alphabetically anyway, so the tiers choose the SET and
 * not the order.
 */
export function matchedEntries(files: FileEntry[], query: string): FileEntry[] {
  const folded = query.trim().toLowerCase();
  if (folded === '') return files;
  return files.filter((file) => fileTier(file, folded) !== null);
}
