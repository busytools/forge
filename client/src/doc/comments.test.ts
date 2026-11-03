import { describe, expect, it } from 'vitest';

import { anchorHash } from '../review/write';
import type { ReviewResolutionRecord, ReviewThreadRecord } from '../session/wire';
import {
  addDraft,
  clearDrafts,
  docScope,
  docThread,
  draftsFor,
  lineContext,
  linesOf,
  placeThreads,
  rowsOf,
  type DocDraft,
} from './comments';

function thread(
  id: string,
  line = 2,
  resolution: ReviewResolutionRecord | null = null,
): ReviewThreadRecord {
  return {
    id,
    anchor: {
      path: '/proj/docs/plan.md',
      side: 'new',
      line,
      content_hash: '42',
      context: ['^ # hi'],
      base_ref: 'doc:/proj/docs/plan.md',
    },
    comments: [{ author: { kind: 'user' }, text: 'note', at: 'x', review_id: null }],
    status: 'open',
    created_at: 'x',
    updated_at: 'x',
    commit: null,
    resolution,
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
  /**
   * **A block owns the blank lines between it and the next block**, so every
   * source line has a gutter target: without them a document with a blank
   * line between every paragraph leaves half its lines uncommentable.
   */
  it('renders markdown as blocks with their source spans, gaps included', () => {
    const rows = rowsOf('# hi\n\nbody **bold**\n', 'markdown');

    expect(rows).toHaveLength(2);
    expect(rows[0]).toMatchObject({ line: 1, end: 2, context: ['# hi'] });
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
   * **A line comment captures from its own line**, the anchor's line first:
   * that first captured line is the content a revision re-finds the thread
   * by, so it must be the line the reader pressed and not a neighbour.
   */
  it('captures a line comment from its own line, the line first', () => {
    const lines = linesOf('one\ntwo\nthree\nfour\n');

    expect(lineContext(lines, 2)).toEqual(['two', 'three', 'four']);
    expect(lineContext(lines, 4), 'the last line captures itself').toEqual(['four']);
  });

  /**
   * **Where the server's landing puts a thread is where it draws.** A moved
   * thread draws at its new line with the old one named; a thread the
   * revision left nowhere draws at its captured line, marked stale; and one
   * whose captured line is past the document has nowhere to draw at all,
   * with the reason said in words.
   */
  it('draws a thread where its landing places it, or stale at its captured line', () => {
    const moved = thread('moved', 2, { state: 'placed', line: 5, moved_from: 2 });
    const gone = thread('gone', 3, {
      state: 'outdated',
      reason: 'gone',
    });
    const past = thread('past', 42, {
      state: 'outdated',
      reason: 'gone',
    });

    const placements = placeThreads([moved, gone, past], 10);

    expect(placements[0]).toMatchObject({
      line: 5,
      movedFrom: 2,
      stale: false,
    });
    expect(placements[1]).toMatchObject({
      line: 3,
      movedFrom: null,
      stale: true,
      why: null,
    });
    expect(placements[2]).toMatchObject({ line: null, stale: true });
    expect(placements[2]?.why).toContain('42');
    expect(placements[2]?.why, 'the sentence names what it is past').toContain('10');
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

    const built = docThread(draft, '/proj/docs/plan.md');

    expect(built.anchor).toEqual({
      path: '/proj/docs/plan.md',
      side: 'New',
      line: 2,
      content_hash: anchorHash('a').toString(),
      context: ['a', 'b'],
      base_ref: 'doc:/proj/docs/plan.md',
    });
    expect(built.comments).toEqual([{ author: 'User', text: 'hi', at: '', review_id: null }]);
    expect(built.status).toBe('Open');
    expect(built.commit).toBeNull();
    expect(built.created_at).toBe('');
  });
});
