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
