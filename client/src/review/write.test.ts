import { describe, expect, it } from 'vitest';

import type { DiffLineRecord, ReviewThreadRecord } from '../session/wire';
import {
  anchorHash,
  captureContext,
  deleteCommand,
  finishRound,
  newThread,
  reopenThread,
  statusCommand,
  submitCommand,
  upsertCommand,
  withEditedTurn,
  withReply,
  type Sends,
  type SendOutcome,
} from './write';

const SLOT = { org: 'Busytools', project: 'forge', label: 'lead' };

/** The seat whose review page is being written - the worker's own, which is
 *  the page a review is keyed to and never the lead's. */
const WORKER = { org: 'Busytools', project: 'forge', label: 'review-nudge' };

/** The page's two ways to the socket, as a case arranges them: every command
 *  is recorded, the `submit_review` answers `body`, `went: false` fails every
 *  write, and `dead: true` makes the nudge's raw dispatch throw like a closed
 *  socket. */
function recordingSends(
  body: unknown = null,
  options: { went?: boolean; dead?: boolean } = {},
): {
  sent: unknown[];
  sends: Sends;
} {
  const sent: unknown[] = [];
  return {
    sent,
    sends: {
      write: (command: unknown): Promise<SendOutcome> => {
        sent.push(command);
        return Promise.resolve({
          went: options.went ?? true,
          body: 'submit_review' in (command as object) ? body : null,
        });
      },
      dispatch: (command: unknown): unknown => {
        sent.push(command);
        if (options.dead === true) throw new Error('the socket is not open');
        return null;
      },
    },
  };
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

  it("replaces one turn's words and leaves the thread's identity alone", () => {
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
    const payload = withEditedTurn(thread, 0, 'why this, actually?');
    expect(payload.comments.map((turn) => turn.text)).toEqual(['why this, actually?', 'fixed']);
    expect(payload.comments[0]?.review_id, 'the edited turn keeps its round').toBe('r1');
    expect(payload.comments[0]?.at, 'and its own clock').toBe('2026-01-01T00:00:00Z');
    expect(payload.comments[1]?.author, 'the turns after it are untouched').toEqual({
      Agent: { label: 'impl' },
    });
    expect(payload.id, "the thread's id is what makes the upsert a replace").toBe('t1');
  });

  it('builds the retraction the cards dispatch', () => {
    expect(deleteCommand(SLOT, 'b', 't1')).toEqual({
      delete_review_thread: { project: 'forge', branch: 'b', thread_id: 't1' },
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
    const { sent, sends } = recordingSends(LANDED);

    const sealed = await finishRound(sends, WORKER, 'work/review-nudge', ['t1'], 'pass one', true);

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
    expect(nudge?.prompt_under?.['attachments'], 'a nudge carries no images').toEqual([]);
    expect(typeof nudge?.prompt_under?.['uuid'], 'the prompt rides its own id').toBe('string');
  });

  /**
   * **A round with nothing left to act on is not worth a nudge.** The view
   * this client replaced sealed by the same predicate: when every sealed
   * thread is resolved or outdated, the finish files the round but tells the
   * worker nothing - a worker called to read a round with no question in it
   * spends a turn learning that.
   */
  it('files a round with no work in it without nudging the seat', async () => {
    const { sent, sends } = recordingSends(LANDED);

    const sealed = await finishRound(sends, WORKER, 'work/review-nudge', ['t1'], null, false);

    expect(sealed, 'the round still sealed').toBe(true);
    expect(sent, 'the seal went and the seat was left alone').toEqual([
      submitCommand(WORKER, 'work/review-nudge', ['t1'], null),
    ]);
  });

  /**
   * A reply that carries no round - the store refused, or the dispatch was
   * turned down - filed nothing, so the seat is left alone (a worker told to
   * go read a review that does not exist is worse off than one told
   * nothing), and the finish did not land: the page keeps the reader's words
   * rather than clearing a round that was never filed, exactly as a write
   * that never left does.
   */
  it('seals nothing and nudges nobody when the reply carried no round', async () => {
    const { sent, sends } = recordingSends(null);

    const sealed = await finishRound(sends, WORKER, 'work/review-nudge', ['t1'], null, true);

    expect(sealed, 'a reply that carried no round sealed nothing, so the words stay').toBe(false);
    expect(sent, 'the submit went and nothing else').toEqual([
      submitCommand(WORKER, 'work/review-nudge', ['t1'], null),
    ]);
  });

  /** A submit that never left nudges nobody either: there is no review to read. */
  it('nudges nobody when the submit never left', async () => {
    const { sent, sends } = recordingSends(null, { went: false });

    const sealed = await finishRound(sends, WORKER, 'work/review-nudge', ['t1'], null, true);

    expect(sealed, 'a submit that never left sealed nothing').toBe(false);
    expect(sent, 'the submit is all that was dispatched').toHaveLength(1);
  });

  /**
   * **A nudge that cannot leave is the worker's own line missing, never a
   * lost review.** A socket that died between the seal's reply and the
   * nudge's send throws at the nudge - and the finish still says it sealed,
   * because it did: the round is filed, the drafts clear, and no refusal is
   * drawn about a courtesy line that could not leave (the document viewer's
   * nudge swallows the same failure for the same reason).
   */
  it('stays sealed when the nudge could not leave', async () => {
    const { sent, sends } = recordingSends(LANDED, { dead: true });

    const sealed = await finishRound(sends, WORKER, 'work/review-nudge', ['t1'], null, true);

    expect(sealed, 'the round sealed; the nudge it lost is not the round').toBe(true);
    expect(sent, 'the nudge was attempted after the seal').toHaveLength(2);
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
    const { sent, sends } = recordingSends();

    const went = await reopenThread(sends, WORKER, 'work/review-nudge', 't1', 2);

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
    expect(nudge?.prompt_under?.['source'], 'the line reads as the reader own words').toBe('you');
  });

  /** A thread no round has filed reopens under the round-less line. */
  it('reopens with the round-less line when no round is filed', async () => {
    const { sent, sends } = recordingSends();

    await reopenThread(sends, WORKER, 'work/review-nudge', 't1', null);

    const nudge = sent[1] as { prompt_under?: Record<string, unknown> } | undefined;
    expect(nudge?.prompt_under?.['text'], 'the words are the view this client replaced sent').toBe(
      'Reopened a review comment - take another look via the review MCP (`review__list`).',
    );
  });

  /** A flip that never left nudges nobody: the thread was not reopened. */
  it('nudges nobody when the flip never left', async () => {
    const { sent, sends } = recordingSends(null, { went: false });

    const went = await reopenThread(sends, WORKER, 'work/review-nudge', 't1', 1);

    expect(went, 'a flip that never left reopened nothing').toBe(false);
    expect(sent, 'the flip is all that was dispatched').toHaveLength(1);
  });

  /** The reopen's nudge is best-effort in the same way the finish's is. */
  it('stays reopened when the nudge could not leave', async () => {
    const { sent, sends } = recordingSends(null, { dead: true });

    const went = await reopenThread(sends, WORKER, 'work/review-nudge', 't1', 1);

    expect(went, 'the thread reopened; the nudge it lost is not the flip').toBe(true);
    expect(sent, 'the nudge was attempted after the flip').toHaveLength(2);
  });
});
