import { describe, expect, it } from 'vitest';

import { filesOnly, type FileEntry } from '../composer/wire';
import { closedFolders, indexTreeRows, foldedByDefault, matchedEntries } from './tree';

function entry(relPath: string): FileEntry {
  return {
    relPath,
    relPathLower: relPath.toLowerCase(),
    basenameLower: (relPath.split('/').pop() ?? relPath).toLowerCase(),
    depth: relPath.split('/').length - 1,
  };
}

describe('the seat index as a tree', () => {
  const files = [
    entry('src/b.ts'),
    entry('src/a.ts'),
    entry('Cargo.toml'),
    entry('crates/z.rs'),
    entry('src/sub/deep.rs'),
  ];

  /**
   * Ved, 2026-10-19: the page opens folded. A folder holding a file is
   * folded; a folder holding only folders opens, so the skeleton reads.
   */
  it('folds the folders that hold a file and opens the folder-only ones', () => {
    const onlyDirs = [entry('apps/web/index.html'), entry('apps/api/main.rs')];
    expect([...foldedByDefault(files)].sort()).toEqual(['crates', 'src', 'src/sub']);
    expect([...foldedByDefault(onlyDirs)].sort()).toEqual(['apps/api', 'apps/web']);

    const rows = indexTreeRows(onlyDirs, foldedByDefault(onlyDirs));
    expect(rows.map((row) => `${row.kind} d${row.depth} ${row.path}`)).toEqual([
      'dir d0 apps',
      'dir d1 apps/api',
      'dir d1 apps/web',
    ]);
  });

  it('a fold the reader set wins over the default, either way', () => {
    const defaults = new Set(['src']);
    expect([...closedFolders(defaults, new Map())]).toEqual(['src']);
    expect([...closedFolders(defaults, new Map([['src', false]]))]).toEqual([]);
    expect([...closedFolders(defaults, new Map([['docs', true]]))].sort()).toEqual(['docs', 'src']);
  });

  it('a folder with files stays folded, so its subfolders start hidden too', () => {
    const rows = indexTreeRows(files, foldedByDefault(files));
    expect(rows.map((row) => `${row.kind} ${row.path}`)).toEqual([
      'dir crates',
      'dir src',
      'file Cargo.toml',
    ]);
  });

  it('draws folders before files at every level, alphabetical, indented by depth', () => {
    const rows = indexTreeRows(files, new Set());
    expect(rows.map((row) => `${row.kind} d${row.depth} ${row.path}`)).toEqual([
      'dir d0 crates',
      'file d1 crates/z.rs',
      'dir d0 src',
      'dir d1 src/sub',
      'file d2 src/sub/deep.rs',
      'file d1 src/a.ts',
      'file d1 src/b.ts',
      'file d0 Cargo.toml',
    ]);
  });

  it('a directory row carries every file beneath it', () => {
    const rows = indexTreeRows(files, new Set());
    const count = (path: string): number | null =>
      rows.find((row) => row.path === path)?.count ?? null;
    expect(count('crates')).toBe(1);
    expect(count('src')).toBe(3);
    expect(count('src/sub')).toBe(1);
    expect(count('src/a.ts'), 'a file row carries no count').toBe(null);
  });

  it('a closed directory keeps its row and hides its subtree', () => {
    const rows = indexTreeRows(files, new Set(['src']));
    expect(rows.some((row) => row.path === 'src')).toBe(true);
    expect(rows.some((row) => row.path === 'src/sub' || row.path === 'src/a.ts')).toBe(false);
    expect(rows.some((row) => row.path === 'src/sub/deep.rs')).toBe(false);
  });

  /**
   * **The walk lists directories too** (a `rel_path` ending in `/`, measured
   * on the live scratch), and a directory entry reaching the tree would
   * draw an empty-named row beside the real one. The page's input drops
   * them first, and the tree derives its own directories.
   */
  it('drops the directory entries the walk lists', () => {
    const withDirs = [entry('src/'), entry('src/lib.rs'), entry('.claude/'), entry('README.md')];
    expect(filesOnly(withDirs).map((file) => file.relPath)).toEqual(['src/lib.rs', 'README.md']);
    const rows = indexTreeRows(filesOnly(withDirs), new Set());
    expect(rows.map((row) => `${row.kind} ${row.path}`)).toEqual([
      'dir src',
      'file src/lib.rs',
      'file README.md',
    ]);
  });
});

describe('the files filter', () => {
  const files = [entry('src/app.test.ts'), entry('docs/plan.md'), entry('crates/forge/deep.rs')];

  it('keeps what the @ list would find, case-folded, on basename or path', () => {
    expect(matchedEntries(files, 'plan').map((file) => file.relPath)).toEqual(['docs/plan.md']);
    expect(matchedEntries(files, 'DEEP').map((file) => file.relPath)).toEqual([
      'crates/forge/deep.rs',
    ]);
    expect(matchedEntries(files, 'src/app').map((file) => file.relPath)).toEqual([
      'src/app.test.ts',
    ]);
  });

  it('an empty query keeps everything and a miss keeps nothing', () => {
    expect(matchedEntries(files, '').length).toBe(3);
    expect(matchedEntries(files, '   ').length).toBe(3);
    expect(matchedEntries(files, 'zzz')).toEqual([]);
  });
});
