import { describe, expect, it } from 'vitest';

import type { ReviewThreadRecord } from '../session/wire';
import { contextForComment, railRows, treeScope } from './comments';

const SLOT = { org: 'Busytools', project: 'forge', label: 'tree-review-survey' };

function thread(
  id: string,
  path: string,
  line: number,
  resolution: ReviewThreadRecord['resolution'],
): ReviewThreadRecord {
  return {
    id,
    anchor: {
      path,
      side: 'new',
      line,
      content_hash: '7',
      context: ['ctx'],
      base_ref: treeScope(SLOT),
    },
    comments: [
      { author: { kind: 'user' }, text: `c${id}`, at: '2026-10-17T10:00:00Z', review_id: null },
    ],
    status: 'open',
    created_at: '2026-10-17T10:00:00Z',
    updated_at: '2026-10-17T10:00:00Z',
    commit: null,
    resolution,
  };
}

describe('the seat tree scope', () => {
  it('is spelled exactly as the crate spells it', () => {
    expect(treeScope(SLOT)).toBe('tree:Busytools/forge/tree-review-survey');
  });
});

describe('a comment anchor captures its neighbours', () => {
  const lines = ['a', 'b', 'c', 'd', 'e', 'f', 'g', 'h'];

  it('takes the crate radius on each side, never the line itself', () => {
    expect(contextForComment(lines, 4)).toEqual(['a', 'b', 'c', 'e', 'f', 'g']);
  });

  it("clamps at the file's own ends", () => {
    expect(contextForComment(lines, 1)).toEqual(['b', 'c', 'd']);
    expect(contextForComment(lines, 8)).toEqual(['e', 'f', 'g']);
  });
});

describe('the rail rows', () => {
  it('orders by path then by the line the landing places', () => {
    const rows = railRows([
      thread('b', 'src/z.rs', 3, { state: 'placed', line: 3, moved_from: null }),
      thread('a', 'src/a.rs', 9, { state: 'placed', line: 9, moved_from: null }),
      thread('c', 'src/a.rs', 2, { state: 'placed', line: 2, moved_from: null }),
    ]);
    expect(rows.map((row) => row.thread.id)).toEqual(['c', 'a', 'b']);
  });

  it('names the recorded line when the landing moved', () => {
    const rows = railRows([
      thread('m', 'src/x.rs', 5, { state: 'placed', line: 7, moved_from: 5 }),
    ]);
    expect(rows[0]?.line).toBe(7);
    expect(rows[0]?.movedFrom).toBe(5);
    expect(rows[0]?.stale).toBe(false);
  });

  it('parks a thread the landing could not place, on its recorded line', () => {
    const parked: ReviewThreadRecord['resolution'][] = [
      { state: 'outdated', reason: 'gone' },
      null,
    ];
    for (const resolution of parked) {
      const rows = railRows([thread('p', 'src/x.rs', 4, resolution)]);
      expect(rows[0]?.line, JSON.stringify(resolution)).toBe(null);
      expect(rows[0]?.stale).toBe(true);
    }
  });
});
