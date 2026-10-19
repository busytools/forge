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
 * the tree's skeleton reads without a click and no file rows inside folders
 * show.
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
const NO_FOLDS: ReadonlySet<string> = new Set<string>();

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
 * The folders drawn shut for `files` under `query`. A filter's job is to
 * show what matched, so every fold stands down while one is on, the
 * reader's own included; clearing it brings their folds back.
 */
export function foldsFor(
  files: FileEntry[],
  query: string,
  folded: ReadonlyMap<string, boolean>,
): ReadonlySet<string> {
  if (query.trim() !== '') return NO_FOLDS;
  return closedFolders(foldedByDefault(files), folded);
}

/** Every folder above `path`, outermost first: what a jump opens. */
export function ancestorsOf(path: string): string[] {
  const parts = path.split('/');
  parts.pop();
  const out: string[] = [];
  let prefix = '';
  for (const part of parts) {
    prefix = prefix === '' ? part : `${prefix}/${part}`;
    out.push(prefix);
  }
  return out;
}

/** No fold the reader set: the state a page starts on. */
export const NO_MANUAL_FOLDS: ReadonlyMap<string, boolean> = new Map();

/** A reader's fold as a new value, so the page's state changes by identity
 *  rather than by mutating the map the derivation read. */
export function withFold(
  folded: ReadonlyMap<string, boolean>,
  path: string,
  shut: boolean,
): ReadonlyMap<string, boolean> {
  const out = new Map(folded);
  out.set(path, shut);
  return out;
}

/**
 * `folded` with every folder above `path` opened, even one the reader had
 * shut by hand: a jump's file must be visible, so its own path wins.
 */
export function openedForJump(
  folded: ReadonlyMap<string, boolean>,
  path: string,
): ReadonlyMap<string, boolean> {
  const out = new Map(folded);
  for (const ancestor of ancestorsOf(path)) out.set(ancestor, false);
  return out;
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
