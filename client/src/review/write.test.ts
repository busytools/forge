import { describe, expect, it } from 'vitest';

import type { DiffLineRecord, ReviewThreadRecord } from '../session/wire';
import {
  anchorHash,
  captureContext,
  newThread,
  statusCommand,
  submitCommand,
  upsertCommand,
  withReply,
} from './write';

const SLOT = { org: 'Busytools', project: 'forge', label: 'lead' };

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
