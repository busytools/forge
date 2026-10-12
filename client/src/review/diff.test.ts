import { describe, expect, it } from 'vitest';

import type { DiffFileRecord } from '../session/wire';
import { filesOf, findLine, lineCounts, synthesizeFileDiff, treeRows } from './diff';

function file(overrides: Partial<DiffFileRecord> = {}): DiffFileRecord {
  return {
    path: 'src/a.rs',
    oldPath: null,
    status: 'modified',
    binary: false,
    submodule: false,
    truncated: false,
    hunks: [
      {
        oldStart: 1,
        oldCount: 3,
        newStart: 1,
        newCount: 4,
        lines: [
          { kind: 'context', text: 'one', oldLine: 1, newLine: 1 },
          { kind: 'removed', text: 'two', oldLine: 2, newLine: null },
          { kind: 'added', text: 'two and a half', oldLine: null, newLine: 2 },
          { kind: 'added', text: 'three', oldLine: null, newLine: 3 },
          { kind: 'context', text: 'four', oldLine: 3, newLine: 4 },
        ],
      },
    ],
    ...overrides,
  };
}

describe('the canvas derivations', () => {
  it('re-emits the hunks as the unified text the viewer parses', () => {
    // The plausible regression is trusting the crate's per-line text to be
    // enough: the viewer parses unified diff text ONLY, so a rebuild that
    // dropped a marker or a range would render lines on the wrong side or
    // not at all. The ranges are the crate's own, copied not recomputed.
    expect(synthesizeFileDiff(file())).toBe(
      [
        'diff --git a/src/a.rs b/src/a.rs',
        '--- a/src/a.rs',
        '+++ b/src/a.rs',
        '@@ -1,3 +1,4 @@',
        ' one',
        '-two',
        '+two and a half',
        '+three',
        ' four',
        '',
      ].join('\n'),
    );
  });

  it('names a rename by both paths, so the header reads it', () => {
    const text = synthesizeFileDiff(file({ oldPath: 'src/old.rs', path: 'src/new.rs' }));
    expect(text.split('\n')[0]).toBe('diff --git a/src/old.rs b/src/new.rs');
  });

  it('counts additions and removals per file, and reads one scope of files', () => {
    const one = file();
    expect(lineCounts(one)).toEqual({ plus: 2, minus: 1 });
    expect(filesOf({ state: 'clean' }), 'a clean layer has nothing to draw').toEqual([]);
    expect(filesOf({ state: 'populated', files: [one] })).toEqual([one]);
    expect(filesOf({ state: 'scan_failed' })).toEqual([]);
  });

  it('draws the scope as its own tree - nested, folders first, depths apart', () => {
    // The plausible regression is the flat grouping this replaced: one row
    // per full-path directory reads as neither a tree nor a path list, and
    // the structure disappears exactly where the paths get deep.
    const root = file({ path: 'top.md' });
    const deep = file({ path: 'src/deep/b.rs' });
    const one = file();
    const rows = treeRows([deep, root, one], new Set());
    expect(rows.map((row) => [row.depth, row.kind, row.name])).toEqual([
      [0, 'dir', 'src'],
      [1, 'dir', 'deep'],
      [2, 'file', 'b.rs'],
      [1, 'file', 'a.rs'],
      [0, 'file', 'top.md'],
    ]);
    expect(rows[0], 'a directory carries every file beneath it').toMatchObject({
      path: 'src',
      count: 2,
    });
    expect(rows[3], 'a file row carries its record and full path').toMatchObject({
      path: 'src/a.rs',
      file: one,
    });
  });

  it('a closed directory hides its subtree and leaves its siblings drawn', () => {
    const rows = treeRows(
      [file(), file({ path: 'src/deep/b.rs' }), file({ path: 'top.md' })],
      new Set(['src/deep']),
    );
    expect(rows.map((row) => [row.kind, row.name])).toEqual([
      ['dir', 'src'],
      ['dir', 'deep'],
      ['file', 'a.rs'],
      ['file', 'top.md'],
    ]);
  });

  it('finds the line a composer click landed on, by side', () => {
    const held = file();
    const removal = findLine(held, 'old', 2);
    expect(removal?.index, 'the removed line is found on the old side').toBe(1);
    const addition = findLine(held, 'new', 2);
    expect(addition?.index, 'and the added one on the new side').toBe(2);
    expect(findLine(held, 'new', 2)?.hunk.newStart).toBe(1);
    expect(findLine(held, 'new', 99), 'a line the diff does not carry').toBeNull();
  });
});
