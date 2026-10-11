import { describe, expect, it } from 'vitest';

import type { DiffFileRecord } from '../session/wire';
import { filesOf, findLine, lineCounts, synthesizeFileDiff, treeOf } from './diff';

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

  it('counts additions and removals per file, and groups the tree by directory', () => {
    const one = file();
    const two = file({ path: 'src/deep/b.rs' });
    const root = file({ path: 'top.md' });
    expect(lineCounts(one)).toEqual({ plus: 2, minus: 1 });
    expect(treeOf([one, two, root])).toEqual([
      ['src', [one]],
      ['src/deep', [two]],
      ['', [root]],
    ]);
    expect(filesOf({ state: 'clean' }), 'a clean layer has nothing to draw').toEqual([]);
    expect(filesOf({ state: 'populated', files: [one] })).toEqual([one]);
    expect(filesOf({ state: 'scan_failed' })).toEqual([]);
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
