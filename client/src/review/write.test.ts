import { describe, expect, it } from 'vitest';

import type { DiffLineRecord, ReviewThreadRecord } from '../session/wire';
import {
  anchorHash,
  captureContext,
  finishRound,
  newThread,
  reopenThread,
  statusCommand,
  submitCommand,
  upsertCommand,
  withReply,
  type SendOutcome,
} from './write';

const SLOT = { org: 'Busytools', project: 'forge', label: 'lead' };

/** The seat whose review page is being written - the worker's own, which is
 *  the page a review is keyed to and never the lead's. */
const WORKER = { org: 'Busytools', project: 'forge', label: 'review-nudge' };

/** A page send: records every command, answers the `submit_review` with
 *  `body`, and answers `went` for every command when a case needs the write
 *  to fail (the shape the page's own `write` returns). */
function recordingSend(
  body: unknown = null,
  went = true,
): {
  sent: unknown[];
  send: (c: unknown) => Promise<SendOutcome>;
} {
  const sent: unknown[] = [];
  const send = (command: unknown): Promise<SendOutcome> => {
    sent.push(command);
    return Promise.resolve({
      went,
      body: 'submit_review' in (command as object) ? body : null,
    });
  };
  return { sent, send };
}

describe('the anchor capture', () => {
  it("hashes the crate's vectors exactly", () => {
    // These three values are pinned in forge-agent's resolver tests
    // (`the_hash_vectors_the_web_client_mirrors`): the page computes the
    // hash for the anchors it writes and the crate's resolver matches by
    // it, so a change to either implementation alone must fail the other's
    // test rather than dropping threads into Gone at run time. The third
    // vector rides a non-ASCII byte, where a port over UTF-16 code units
    // instead of UTF-8 drifts.
    expect(anchorHash('let x = 1;').toString()).toBe('12051066714322805831');
    expect(anchorHash('  a  b  ').toString(), 'whitespace collapses before hashing').toBe(
      '16591147884985137298',
    );
    expect(anchorHash('fn café() {}').toString()).toBe('14060598129659224412');
    // The collapse's edges, where JS's `\s` and Rust's `split_whitespace`
    // disagree: NEL is White_Space and splits, the BOM is not and stays.
    // Both spelled by codepoint, because both are invisible in an editor.
    expect(
      anchorHash(`a${String.fromCodePoint(0x85)}b`).toString(),
      'NEL collapses like Rust splits it',
    ).toBe('16591147884985137298');
    expect(
      anchorHash(`a${String.fromCodePoint(0xfeff)}b`).toString(),
      'the BOM is content, not whitespace',
    ).toBe('4988096857352653883');
  });

  it("captures the neighbours the crate's radius takes", () => {
    const line = (text: string): DiffLineRecord => ({
      kind: 'context',
      text,
      oldLine: 1,
      newLine: 1,
    });
    const lines = ['one', 'two', 'three', 'four', 'five', 'six', 'seven'].map(line);
    expect(captureContext(lines, 3), 'three each side, the line itself skipped').toEqual([
      'one',
      'two',
      'three',
      'five',
      'six',
      'seven',
    ]);
    expect(captureContext(lines, 0), 'a hunk edge takes what it has').toEqual([
      'two',
      'three',
      'four',
    ]);
  });
});

describe('the commands the page writes', () => {
  it("carries a new comment as the store's own thread shape", () => {
    const thread = newThread({
      path: 'a.rs',
      side: 'new',
      line: 4,
      text: 'why this?',
      context: ['ctx'],
      baseRef: 'main',
      commit: null,
      lineText: 'let x = 1;',
    });
    expect(thread.anchor, 'the hash crosses as digits, exact past 2^53').toEqual({
      path: 'a.rs',
      side: 'New',
      line: 4,
      content_hash: '12051066714322805831',
      context: ['ctx'],
      base_ref: 'main',
    });
    expect(thread.comments).toEqual([
      { author: 'User', text: 'why this?', at: '', review_id: null },
    ]);
    expect(thread.status, 'a fresh comment is open').toBe('Open');
    expect(thread.commit, 'all changes').toBeNull();

    const command = upsertCommand(SLOT, 'work/review-page', thread);
    expect(command.upsert_review_thread.project).toBe('forge');
    expect(command.upsert_review_thread.thread.id).toBe(thread.id);
  });

  it('appends a reply without disturbing what is already on the thread', () => {
    const thread: ReviewThreadRecord = {
      id: 't1',
      anchor: {
        path: 'a.rs',
        side: 'new',
        line: 4,
        content_hash: '12051066714322805831',
        context: ['ctx'],
        base_ref: 'main',
      },
      comments: [
        {
          author: { kind: 'user' },
          text: 'why this?',
          at: '2026-01-01T00:00:00Z',
          review_id: 'r1',
        },
        {
          author: { kind: 'agent', label: 'impl' },
          text: 'fixed',
          at: '2026-01-01T01:00:00Z',
          review_id: null,
        },
      ],
      status: 'addressed',
      created_at: '2026-01-01T00:00:00Z',
      updated_at: '2026-01-01T01:00:00Z',
      commit: null,
      resolution: null,
    };
    const payload = withReply(thread, 'still off');
    expect(payload.comments.map((turn) => turn.text)).toEqual(['why this?', 'fixed', 'still off']);
    expect(payload.comments[0]?.review_id, 'a filed turn keeps its round').toBe('r1');
    expect(
      payload.comments[2]?.review_id,
      'and the reply is unfiled until the round seals',
    ).toBeNull();
    expect(payload.comments[1]?.author, 'an agent turn keeps its label').toEqual({
      Agent: { label: 'impl' },
    });
    expect(payload.anchor.content_hash, 'the hash echoes exactly').toBe('12051066714322805831');
    expect(payload.status, 'the state is carried as it stands').toBe('Addressed');
    expect(payload.commit).toBeNull();
  });

  it('builds the submit and the status commands the socket dispatches', () => {
    expect(submitCommand(SLOT, 'b', ['t1', 't2'], 'pass one')).toEqual({
      submit_review: {
        project: 'forge',
        branch: 'b',
        summary: 'pass one',
        thread_ids: ['t1', 't2'],
        origin: SLOT,
      },
    });
    expect(statusCommand(SLOT, 'b', 't1', 'Resolved')).toEqual({
      set_review_thread_status: {
        project: 'forge',
        branch: 'b',
        thread_id: 't1',
        status: 'Resolved',
      },
    });
  });
});

describe('the finish', () => {
  /** A landed submit's reply: the minted round, as the socket delivers it. */
  const LANDED = { id: 'r9', number: 4, summary: null, created_at: 'x' };

  /**
   * **The nudge is what the view this client replaced sent on its own seal**:
   * a prompt into the session whose branch the review belongs to, one line
   * naming the round and pointing at the review MCP - so the worker hears a
   * review is ready and reads it back, live and on a fresh read.
   */
  it('seals the round, then nudges the seat with the line the terminal sent', async () => {
    const { sent, send } = recordingSend(LANDED);

    const sealed = await finishRound(send, WORKER, 'work/review-nudge', ['t1'], 'pass one');

    expect(sealed, 'the round sealed').toBe(true);
    expect(sent[0], 'the round is sealed first').toEqual(
      submitCommand(WORKER, 'work/review-nudge', ['t1'], 'pass one'),
    );
    const nudge = sent[1] as { prompt_under?: Record<string, unknown> } | undefined;
    expect(nudge?.prompt_under, 'the nudge was not dispatched').toBeDefined();
    expect(
      nudge?.prompt_under?.['key'],
      'the nudge goes to the seat the review belongs to',
    ).toEqual(WORKER);
    expect(nudge?.prompt_under?.['text'], 'the words are the view this client replaced sent').toBe(
      'Review #4 ready - address it via the review MCP (`review__list`).',
    );
    expect(nudge?.prompt_under?.['source'], 'the bubble reads as the reader own words').toBe('you');
    expect(nudge?.prompt_under?.['attachments']).toEqual([]);
    expect(typeof nudge?.prompt_under?.['uuid'], 'the prompt rides its own id').toBe('string');
  });

  /**
   * A reply that carries no round - the store refused, or the dispatch was
   * turned down - filed nothing to nudge anyone about, so the seat is left
   * alone: a worker told to go read a review that does not exist is worse
   * off than one told nothing.
   */
  it('nudges nobody when the reply carried no round', async () => {
    const { sent, send } = recordingSend(null);

    await finishRound(send, WORKER, 'work/review-nudge', ['t1'], null);

    expect(sent, 'the submit went and nothing else').toEqual([
      submitCommand(WORKER, 'work/review-nudge', ['t1'], null),
    ]);
  });

  /** A submit that never left nudges nobody either: there is no review to read. */
  it('nudges nobody when the submit never left', async () => {
    const { sent, send } = recordingSend(null, false);

    const sealed = await finishRound(send, WORKER, 'work/review-nudge', ['t1'], null);

    expect(sealed, 'a submit that never left sealed nothing').toBe(false);
    expect(sent, 'the submit is all that was dispatched').toHaveLength(1);
  });
});

describe('the reopen', () => {
  /**
   * **The reopen is the other trigger the view this client replaced nudged
   * on**: flipping a thread back to open tells the worker to look again, and
   * the line names the round the reviewer is unhappy with - the latest one
   * the thread is filed into, not its origin.
   */
  it('flips the thread open, then nudges the seat with the round the reopen is about', async () => {
    const { sent, send: write } = recordingSend();

    const went = await reopenThread(write, WORKER, 'work/review-nudge', 't1', 2);

    expect(went, 'the flip went').toBe(true);
    expect(sent[0], 'the flip is dispatched first').toEqual(
      statusCommand(WORKER, 'work/review-nudge', 't1', 'Open'),
    );
    const nudge = sent[1] as { prompt_under?: Record<string, unknown> } | undefined;
    expect(nudge?.prompt_under, 'the nudge was not dispatched').toBeDefined();
    expect(
      nudge?.prompt_under?.['key'],
      'the nudge goes to the seat the review belongs to',
    ).toEqual(WORKER);
    expect(nudge?.prompt_under?.['text'], 'the words are the view this client replaced sent').toBe(
      'Reopened a comment in review #2 - take another look via the review MCP (`review__get`).',
    );
    expect(nudge?.prompt_under?.['source']).toBe('you');
  });

  /** A thread no round has filed reopens under the round-less line. */
  it('reopens with the round-less line when no round is filed', async () => {
    const { sent, send: write } = recordingSend();

    await reopenThread(write, WORKER, 'work/review-nudge', 't1', null);

    const nudge = sent[1] as { prompt_under?: Record<string, unknown> } | undefined;
    expect(nudge?.prompt_under?.['text'], 'the words are the view this client replaced sent').toBe(
      'Reopened a review comment - take another look via the review MCP (`review__list`).',
    );
  });

  /** A flip that never left nudges nobody: the thread was not reopened. */
  it('nudges nobody when the flip never left', async () => {
    const { sent, send: write } = recordingSend(null, false);

    const went = await reopenThread(write, WORKER, 'work/review-nudge', 't1', 1);

    expect(went, 'a flip that never left reopened nothing').toBe(false);
    expect(sent, 'the flip is all that was dispatched').toHaveLength(1);
  });
});
