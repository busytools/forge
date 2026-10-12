import { describe, expect, it } from 'vitest';

import type { ReviewThreadRecord } from '../session/wire';
import {
  addDraft,
  clearDrafts,
  docScope,
  docThread,
  draftsFor,
  linesOf,
  placedThreads,
  rowsOf,
  threadsAgainstAnotherVersion,
  type DocDraft,
} from './comments';

function thread(id: string, hash: string, line = 2): ReviewThreadRecord {
  return {
    id,
    anchor: {
      path: '/proj/docs/plan.md',
      side: 'new',
      line,
      content_hash: hash,
      context: ['^ # hi'],
      base_ref: 'doc:/proj/docs/plan.md',
    },
    comments: [{ author: { kind: 'user' }, text: 'note', at: 'x', review_id: null }],
    status: 'open',
    created_at: 'x',
    updated_at: 'x',
    commit: null,
    resolution: null,
  };
}

describe('the document comment helpers', () => {
  it('numbers the document lines from one, keeping the blank ones', () => {
    expect(linesOf('# hi\n\nbody\n')).toEqual([
      { number: 1, text: '# hi' },
      { number: 2, text: '' },
      { number: 3, text: 'body' },
    ]);
  });

  /**
   * **A markdown document's rows are its rendered blocks**, each carrying
   * the source lines it came from: the first line is the anchor, and the
   * whole span is the comment's captured context - what keeps a comment's
   * lines exact when the document changes.
   */
  it('renders markdown as blocks with their source spans', () => {
    const rows = rowsOf('# hi\n\nbody **bold**\n', 'markdown');

    expect(rows).toHaveLength(2);
    expect(rows[0]).toMatchObject({ line: 1, end: 1, context: ['# hi'] });
    expect(rows[0]?.html, 'a heading renders as a heading').toContain('<h1>');
    expect(rows[1]).toMatchObject({ line: 3, end: 3, context: ['body **bold**'] });
    expect(rows[1]?.html).toContain('<strong>bold</strong>');
  });

  it('keeps a multi-line block whole, with its lines captured', () => {
    const rows = rowsOf('one\ntwo\nthree\n', 'markdown');

    expect(rows).toHaveLength(1);
    expect(rows[0]).toMatchObject({ line: 1, end: 3, context: ['one', 'two', 'three'] });
  });

  /** A plain-text document has no blocks: its rows are its own lines, drawn
   *  as the text they are. */
  it('makes text documents a row per line', () => {
    const rows = rowsOf('alpha\n\nbeta', 'text');

    expect(rows).toHaveLength(3);
    expect(rows[1]).toMatchObject({ line: 2, end: 2, context: [''], html: null, text: '' });
    expect(rows[2]?.text).toBe('beta');
  });

  it('spells the scope the store keys the document under', () => {
    expect(docScope('/proj/docs/plan.md')).toBe('doc:/proj/docs/plan.md');
  });

  /**
   * **The document's own hash is the line's placement test.** A thread
   * written against this exact content sits at its line; one written
   * against another version is outdated AT ONCE - nothing re-anchors a
   * document automatically - with its captured lines kept.
   */
  it('splits placed threads from ones written against another version', () => {
    const threads = [thread('a', '42'), thread('b', '41'), thread('c', '42', 5)];

    expect(placedThreads(threads, '42').map((t) => t.id)).toEqual(['a', 'c']);
    expect(threadsAgainstAnotherVersion(threads, '42').map((t) => t.id)).toEqual(['b']);
  });

  it('drafts survive the viewer leaving the document and coming back', () => {
    const scope = docScope('/a.md');
    const draft: DocDraft = { id: 'x', line: 2, text: 'hi', context: ['one'] };

    addDraft(scope, draft);
    expect(draftsFor(scope).map((held) => held.id)).toEqual(['x']);

    // The finish clears only what it filed.
    clearDrafts(scope, ['x']);
    expect(draftsFor(scope)).toEqual([]);
  });

  it('builds the thread payload the store takes, anchored to the document', () => {
    const draft: DocDraft = { id: 'x', line: 2, text: 'hi', context: ['a', 'b'] };

    const built = docThread(draft, '/proj/docs/plan.md', '42');

    expect(built.anchor).toEqual({
      path: '/proj/docs/plan.md',
      side: 'New',
      line: 2,
      content_hash: '42',
      context: ['a', 'b'],
      base_ref: 'doc:/proj/docs/plan.md',
    });
    expect(built.comments).toEqual([{ author: 'User', text: 'hi', at: '', review_id: null }]);
    expect(built.status).toBe('Open');
    expect(built.commit).toBeNull();
    expect(built.created_at).toBe('');
  });
});
