import { describe, expect, it } from 'vitest';

import { backgroundTaskFrom, diffFrom, gitFrom, reviewThreadFrom, reviewsFrom } from './wire';

describe('the background-task registry parse', () => {
  it('drops a row missing its id, its kind or its words rather than drawing it blank', () => {
    // The plausible regression is a forgiving parse (`?? ''`), which draws a
    // row a reader cannot act on and whose absence nothing else would report.
    expect(backgroundTaskFrom({ task_type: 'local_bash', description: 'x' }), 'no id').toEqual([]);
    expect(backgroundTaskFrom({ task_id: 't', description: 'x' }), 'no kind').toEqual([]);
    expect(backgroundTaskFrom({ task_id: 't', task_type: 'local_bash' }), 'no words').toEqual([]);
    expect(
      backgroundTaskFrom({ task_id: 't', task_type: 'local_bash', description: 'x' }),
      'a full row parses, with no command',
    ).toEqual([
      { task_id: 't', task_type: 'local_bash', description: 'x', command: null, tool_use_id: null },
    ]);
  });
});

describe('a git record a server never sent', () => {
  /**
   * The fold's absent case: an older server states no git at all, and the row
   * must read as nothing to state rather than as a failure. The only pin of
   * this branch went with the floor fixture, and it matters more now - a
   * server on another protocol connects and is folded rather than refused.
   */
  it('reads an absent git record as nothing to state', () => {
    expect(gitFrom(undefined), 'no git at all').toEqual({
      defaultBranch: null,
      worktree: null,
      ahead: null,
    });
  });
});

describe('the reviews parse', () => {
  it('reads the fixture shape: both halves read, each with its own outcome', () => {
    // The plausible regression is wiring the two halves to each other's key,
    // or collapsing them into one - which would make a corrupt reviews row
    // start failing a threads-only reader, the thing the server keeps apart.
    const held = reviewsFrom({
      threads: { outcome: 'read', value: [] },
      reviews: { outcome: 'read', value: [] },
    });
    expect(held, 'both halves read').toEqual({
      threads: { outcome: 'read', value: [] },
      reviews: { outcome: 'read', value: [] },
    });
  });

  it('keeps one half failing while the other reads', () => {
    const held = reviewsFrom({
      threads: { outcome: 'failed', value: 'decode error' },
      reviews: { outcome: 'read', value: [] },
    });
    expect(held.threads, 'the failing half keeps its reason').toEqual({
      outcome: 'failed',
      value: 'decode error',
    });
    expect(held.reviews, 'the other half is untouched by it').toEqual({
      outcome: 'read',
      value: [],
    });
  });

  it('keeps an outcome, a status, a side and an author it cannot name', () => {
    // The no-drop rule at the boundary: a core ahead of this build answers
    // with a word this parse does not know, and folding it into `failed` (or
    // into a neighbouring state) would draw a confident untruth.
    const held = reviewsFrom({
      threads: { outcome: 'reticulating', value: null },
      reviews: { outcome: 'read', value: [] },
    });
    expect(held.threads, 'an unnamed outcome is kept as itself').toEqual({
      outcome: 'unknown',
      value: 'reticulating',
    });

    const thread = reviewThreadFrom({
      id: 't1',
      anchor: {
        path: 'a.rs',
        side: 'Both',
        line: 3,
        content_hash: '1',
        context: ['x', 7],
        base_ref: 'main',
      },
      comments: [{ author: { Robot: { id: 3 } }, text: 'hm', at: '2026-01-01T00:00:00Z' }],
      status: 'Pondered',
      created_at: '2026-01-01T00:00:00Z',
      updated_at: '2026-01-01T00:00:00Z',
    });
    expect(thread.status, 'an unnamed state is kept').toEqual({ unknown: 'Pondered' });
    expect(thread.anchor.side, 'an unnamed side is kept').toEqual({ unknown: 'Both' });
    expect(thread.comments[0]?.author, 'an unnamed author keeps its raw shape').toEqual({
      kind: 'unknown',
      raw: { Robot: { id: 3 } },
    });
    expect(thread.anchor.context, 'a non-string context entry is not a line').toEqual(['x']);
  });

  it('reads an absent review_id as a turn filed in no round', () => {
    // The serde default: a turn stored before membership moved onto it has no
    // key, and it must read as unfiled rather than as a malformed turn.
    const thread = reviewThreadFrom({
      id: 't1',
      anchor: {
        path: 'a.rs',
        side: 'New',
        line: 1,
        content_hash: '1',
        context: [],
        base_ref: 'main',
      },
      comments: [{ author: 'User', text: 'hm', at: '2026-01-01T00:00:00Z' }],
      status: 'Open',
      created_at: '2026-01-01T00:00:00Z',
      updated_at: '2026-01-01T00:00:00Z',
    });
    expect(thread.comments[0]?.review_id, 'no review_id key').toBeNull();
    expect(thread.comments[0]?.author, 'the reviewer reads as the user').toEqual({ kind: 'user' });
    expect(thread.status, "and the state reads in this build's spelling").toBe('open');
  });

  it('reads the landing beside the thread: placed, moved, or outdated why', () => {
    // The plausible regression is dropping `resolution` (it sits beside the
    // thread's own fields, flattened, and reads like a foreign key) - which
    // would leave every all-changes thread unplaceable and silently parked.
    const base = {
      id: 't1',
      anchor: {
        path: 'a.rs',
        side: 'New',
        line: 1,
        content_hash: '1',
        context: [],
        base_ref: 'main',
      },
      comments: [],
      status: 'Open',
      created_at: '2026-01-01T00:00:00Z',
      updated_at: '2026-01-01T00:00:00Z',
      commit: null,
    };
    expect(
      reviewThreadFrom({ ...base, resolution: { state: 'placed', line: 9, moved_from: 4 } })
        .resolution,
      'a moved landing keeps both numbers',
    ).toEqual({ state: 'placed', line: 9, moved_from: 4 });
    expect(
      reviewThreadFrom({ ...base, resolution: { state: 'placed', line: 9, moved_from: null } })
        .resolution,
      'an in-place one has no origin to report',
    ).toEqual({ state: 'placed', line: 9, moved_from: null });
    expect(
      reviewThreadFrom({
        ...base,
        resolution: { state: 'outdated', reason: { ambiguous: { matches: 3 } } },
      }).resolution,
      'an ambiguous reason keeps its count',
    ).toEqual({ state: 'outdated', reason: { ambiguous: { matches: 3 } } });
    expect(
      reviewThreadFrom({
        ...base,
        resolution: { state: 'outdated', reason: 'gone' },
      }).resolution,
    ).toEqual({ state: 'outdated', reason: 'gone' });
    expect(
      reviewThreadFrom({ ...base, resolution: null }).resolution,
      'a commit-scoped thread carries none',
    ).toBeNull();
    expect(
      reviewThreadFrom({ ...base, resolution: { state: 'vibing' } }).resolution,
      'and a shape this build cannot name is kept rather than guessed',
    ).toEqual({ state: 'unknown', raw: 'vibing' });
  });
});

describe('the diff parse', () => {
  it('reads the layers, the hunks and the lines, keeping every state named', () => {
    const diff = diffFrom({
      worktree: {
        populated: [
          {
            path: 'a.rs',
            old_path: null,
            status: 'modified',
            binary: false,
            submodule: false,
            truncated: false,
            hunks: [
              {
                old_start: 1,
                old_count: 2,
                new_start: 1,
                new_count: 3,
                lines: [
                  { kind: 'context', text: 'one', old_line: 1, new_line: 1 },
                  { kind: 'added', text: 'two', old_line: null, new_line: 2 },
                  { kind: 'removed', text: 'gone', old_line: 2, new_line: null },
                ],
              },
            ],
          },
        ],
      },
      branch_ahead: 'clean',
      truncated: true,
    });
    expect(diff.branchAhead, 'a clean layer is nothing to draw').toEqual({ state: 'clean' });
    expect(diff.truncated).toBe(true);
    expect(diff.worktree.state).toBe('populated');
    if (diff.worktree.state !== 'populated') return;
    const file = diff.worktree.files[0];
    expect(file?.status).toBe('modified');
    expect(file?.hunks[0]?.lines[1], 'an added line has no old number').toEqual({
      kind: 'added',
      text: 'two',
      oldLine: null,
      newLine: 2,
    });
    expect(file?.hunks[0]?.lines[2]?.oldLine, 'a removed line has no new one').toBe(2);
  });

  it('names a state it cannot read rather than drawing an empty tree', () => {
    // The plausible regression is folding an unnamed layer state into
    // `clean`: an empty changes list and an unread scan draw the same
    // "nothing here" and mean opposite things.
    expect(diffFrom({ worktree: 'scan_failed', branch_ahead: 'sulking' }).worktree).toEqual({
      state: 'scan_failed',
    });
    expect(
      diffFrom({ worktree: 'sulking', branch_ahead: 'clean' }).worktree,
      'and a state this build cannot name is kept as itself',
    ).toEqual({ state: 'unknown', raw: 'sulking' });
    expect(diffFrom({}).worktree, 'an absent layer is named, not invented clean').toEqual({
      state: 'unknown',
      raw: 'absent',
    });
  });
});
