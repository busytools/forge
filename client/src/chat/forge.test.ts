import { describe, expect, it } from 'vitest';

import { forgeCardOf, type ForgeCard, type ForgeListItem, type ForgePiece } from './forge';

/** A tool result as the wire delivers one: text parts, and whether it failed. */
function result(text: string, is_error = false) {
  return { content: [{ type: 'text', text }], is_error };
}

/** A task record as the tools echo one. */
const RECORD = {
  id: 't-1',
  project: 'forge',
  subject: 'Wire the review notice into the chat',
  status: 'in_progress',
  owner: 'lead',
  estimate: '2h',
  created_at: '2026-10-06T00:12:00Z',
  updated_at: '2026-10-06T09:30:00Z',
};

/** A cron entry as the tools echo one. */
const ENTRY = {
  id: 'c1',
  project: 'forge',
  schedule: { recurring: '0 9 * * *' },
  prompt: 'Summarise overnight CI and open PRs',
  next_fire: '2030-01-01T09:00:00Z',
  description: 'Morning summary',
};

/** A gotify subscription as the tools answer one. */
const SUB = {
  id: '8f1c2d3e-0000-0000-0000-000000000000',
  applications: ['Backups', 'Alerts'],
  min_priority: 5,
  names_resolve: true,
};

/** The kv pairs across every piece a card's body holds. */
function pairsOf(card: ForgeCard | null): unknown[][] {
  return (card?.pieces ?? []).flatMap((piece) =>
    piece.kind === 'kv' ? piece.pairs.map((pair) => [...pair]) : [],
  );
}

/** The items of a card's first list piece, or an empty list. */
function itemsOf(card: ForgeCard | null): ForgeListItem[] {
  const list = (card?.pieces ?? []).find(
    (piece): piece is Extract<ForgePiece, { kind: 'list' }> => piece.kind === 'list',
  );
  return list?.items ?? [];
}

describe('a forge call the page cannot dress', () => {
  it('falls back to the raw text for every case that is not a card', () => {
    const update = 'mcp__forge__tasks__update';
    // A payload that PARSES, so each fallback is its guard's doing and not a
    // malformed body every return path would refuse anyway.
    const wellFormed = JSON.stringify(RECORD);

    expect(forgeCardOf(update, {}, undefined), 'no result yet').toBeNull();
    expect(forgeCardOf(update, {}, result(wellFormed, true)), 'a failed call').toBeNull();
    expect(forgeCardOf(update, {}, result('not json at all')), 'malformed').toBeNull();
    expect(forgeCardOf(update, {}, result('{"id":"t-1"}')), 'no subject to name it by').toBeNull();
    expect(
      forgeCardOf('mcp__forge__tasks__something_new', {}, result(wellFormed)),
      'a verb this family has no arm for',
    ).toBeNull();
    expect(
      forgeCardOf('mcp__playwright__browser_click', {}, result(wellFormed)),
      'a call that is not forge',
    ).toBeNull();
  });
});

describe('the tasks card', () => {
  it('names an update by the subject the echo carries, and says what moved', () => {
    const card = forgeCardOf(
      'mcp__forge__tasks__update',
      { id: 't-1', status: 'in_progress' },
      result(JSON.stringify(RECORD)),
    );
    expect(card, 'the record reads as a card').not.toBeNull();
    expect(card?.title, 'titled by the subject, not the tool').toBe(RECORD.subject);
    expect(card?.chips, 'the state the record now holds').toEqual([
      { text: 'in progress', tone: 'info' },
    ]);
    expect(card?.figure, "the owner is the figure at the row's right").toBe('owner lead');
    // The stamp is the READER's wall clock, so the assertion holds its shape
    // and not its value: a fixed value here would pass in one zone and fail
    // in the next.
    const pairs = pairsOf(card);
    expect(pairs[0], 'the patch the record cannot show').toEqual(['changed', 'status']);
    expect(pairs.slice(1, 3), "then the record's own facts").toEqual([
      ['owner', 'lead'],
      ['estimate', '2h'],
    ]);
    expect(pairs[3], 'and when it last moved, as a readable local time').toEqual([
      'updated',
      expect.stringMatching(/^\d{4}-\d{2}-\d{2} \d{2}:\d{2}$/),
    ]);
  });

  it('draws a detail as prose above the facts, not as one more pair', () => {
    // The detail is the one free-prose field a task carries: an instance's row
    // splits its brief from its facts the same way.
    const stated = { ...RECORD, detail: 'One agents family.' };
    const card = forgeCardOf('mcp__forge__tasks__create', {}, result(JSON.stringify(stated)));
    expect(card?.pieces[0]).toEqual({ kind: 'quote', text: 'One agents family.' });
    expect(
      pairsOf(card).map((pair) => pair[0]),
      'and the detail is not repeated in the meta line',
    ).not.toContain('detail');
  });

  it('names a delete by the removed subject and counts the subtasks that went', () => {
    const deleted = (descendants: number) =>
      forgeCardOf(
        'mcp__forge__tasks__delete',
        { id: 't-1' },
        result(
          JSON.stringify({ status: 'deleted', removed: RECORD, descendants_removed: descendants }),
        ),
      );
    expect(deleted(2)?.title).toBe(RECORD.subject);
    expect(deleted(2)?.chips).toEqual([{ text: 'removed', tone: 'dim' }]);
    expect(deleted(2)?.figure, "the cascade's own count").toBe('with 2 subtasks');
    expect(deleted(1)?.figure, 'one subtask is not two subtasks').toBe('with 1 subtask');
    expect(deleted(0)?.figure, 'a leaf says nothing about subtasks').toBeNull();
  });

  it("lists a project's tasks as rows, each naming its own state", () => {
    const rows = [RECORD, { ...RECORD, id: 't-2', subject: 'Sweep', status: 'pending' }];
    const card = forgeCardOf('mcp__forge__tasks__list', {}, result(JSON.stringify(rows)));
    expect(card?.title).toBe('tasks');
    expect(card?.figure).toBe('2 in flight');
    expect(itemsOf(card)).toEqual([
      {
        id: 't-1',
        state: { text: 'in progress', tone: 'info' },
        text: RECORD.subject,
        tag: null,
        when: 'lead',
      },
      {
        id: 't-2',
        state: { text: 'pending', tone: 'dim' },
        text: 'Sweep',
        tag: null,
        when: 'lead',
      },
    ]);
  });

  it('draws a list of nothing as a row, not as an empty box', () => {
    const card = forgeCardOf('mcp__forge__tasks__list', {}, result('[]'));
    expect(card?.title).toBe('tasks');
    expect(card?.figure, 'no count for no rows').toBeNull();
    expect(card?.pieces).toEqual([]);
  });
});

describe('the cron card', () => {
  it('titles a create by its description and chips the raw expression', () => {
    const card = forgeCardOf('mcp__forge__cron__create', {}, result(JSON.stringify(ENTRY)));
    expect(card?.title, 'the description headlines the row').toBe('Morning summary');
    expect(card?.chips, 'the expression, unglossed').toEqual([
      { text: '0 9 * * *', tone: 'plain' },
    ]);
    expect(card?.figure, 'how long until it fires').toMatch(/^next /);
    expect(pairsOf(card), 'where it fires, and when, in the reader own clock').toEqual([
      ['fires into', 'this session'],
      ['next fire', expect.stringMatching(/^\d{4}-\d{2}-\d{2} \d{2}:\d{2} \(\d{2}:\d{2} UTC\)$/)],
    ]);
    expect(card?.pieces.at(-1), 'and the prompt is the quote').toEqual({
      kind: 'quote',
      text: ENTRY.prompt,
    });
  });

  it('falls back to the prompt first line when no description was given', () => {
    const bare = { ...ENTRY, description: undefined, prompt: 'Check the deploy\nand report' };
    const card = forgeCardOf('mcp__forge__cron__create', {}, result(JSON.stringify(bare)));
    expect(card?.title, 'the prompt first line headlines it').toBe('Check the deploy');
    expect(card?.pieces.at(-1), 'and the whole prompt is still the quote').toEqual({
      kind: 'quote',
      text: bare.prompt,
    });
  });

  it('reads a run-once as once, with its own time', () => {
    const once = { ...ENTRY, schedule: { once_at: '2030-01-01T09:00:00Z' } };
    const card = forgeCardOf('mcp__forge__cron__create', {}, result(JSON.stringify(once)));
    expect(card?.chips[0]?.text).toMatch(/^once · \d{4}-\d{2}-\d{2} \d{2}:\d{2}$/);
  });

  it('names a delete by the entry it removed', () => {
    const card = forgeCardOf(
      'mcp__forge__cron__delete',
      { id: 'c1' },
      result(JSON.stringify({ status: 'deleted', removed: ENTRY })),
    );
    expect(card?.title).toBe('Morning summary');
    expect(card?.chips).toEqual([{ text: 'removed', tone: 'dim' }]);
    expect(card?.pieces).toEqual([{ kind: 'kv', pairs: [['removed', '0 9 * * *']] }]);
  });

  it('lists the schedules with their expressions and next fires', () => {
    const card = forgeCardOf('mcp__forge__cron__list', {}, result(JSON.stringify([ENTRY])));
    expect(card?.title).toBe('schedules');
    expect(card?.figure).toBe('1 registered');
    expect(itemsOf(card)[0]).toMatchObject({ text: 'Morning summary', tag: '0 9 * * *' });
  });
});

describe('the gotify card', () => {
  it('draws a subscription as its own filter', () => {
    const card = forgeCardOf('mcp__forge__gotify__subscribe', {}, result(JSON.stringify(SUB)));
    expect(card?.title).toBe('watching notifications');
    expect(card?.chips, 'the apps and the floor, each as its own words').toEqual([
      { text: 'Backups, Alerts', tone: 'plain' },
      { text: 'priority \u{2265} 5', tone: 'plain' },
    ]);
    expect(card?.figure, 'a resolvable filter says nothing about the index').toBeNull();
    expect(pairsOf(card)[0]).toEqual(['subscription', '8f1c…']);
  });

  it('reports a degraded index as a field and a warning, not only a figure', () => {
    const degraded = { ...SUB, names_resolve: false };
    const card = forgeCardOf('mcp__forge__gotify__subscribe', {}, result(JSON.stringify(degraded)));
    expect(card?.figure, 'the row says the index is stale').toBe('index stale');
    expect(card?.pieces[0], 'and the body says what that costs').toMatchObject({
      kind: 'warnline',
      label: 'warning',
    });
  });

  it('names an unsubscribe by the filter that stopped', () => {
    const card = forgeCardOf(
      'mcp__forge__gotify__unsubscribe',
      { id: SUB.id },
      result(JSON.stringify({ status: 'deleted', removed: SUB })),
    );
    expect(card?.title).toBe('stopped watching');
    expect(card?.chips).toEqual([
      { text: 'Backups, Alerts', tone: 'dim' },
      { text: 'priority \u{2265} 5', tone: 'dim' },
    ]);
  });

  it('lists subscriptions, saying any where nothing is filtered', () => {
    const any = {
      id: 'b20d0000-0000-0000-0000-000000000000',
      applications: [],
      min_priority: null,
    };
    const card = forgeCardOf('mcp__forge__gotify__list', {}, result(JSON.stringify([SUB, any])));
    expect(card?.figure).toBe('2 active');
    expect(itemsOf(card)[1], 'a filter with nothing set says so in both halves').toMatchObject({
      text: 'any application',
      tag: 'any priority',
    });
  });

  it('lists recent notifications, with an elevated priority as a word', () => {
    const rows = [
      { app: 'Backups', title: 'Nightly backup done', priority: 3, date: '2026-10-06T09:00:00Z' },
      { app: 'Alerts', title: 'Volume 3 failed', priority: 8, date: '2026-10-06T09:05:00Z' },
    ];
    const card = forgeCardOf('mcp__forge__gotify__recent', {}, result(JSON.stringify(rows)));
    expect(card?.figure).toBe('2');
    expect(itemsOf(card)[0]).toMatchObject({
      text: 'Nightly backup done',
      tag: 'Backups',
      state: null,
    });
    expect(itemsOf(card)[1]?.state, 'an elevated priority carries its own word').toEqual({
      text: 'priority 8',
      tone: 'warn',
    });
  });
});

describe('the slack card', () => {
  const subscription = (id: string, target: unknown) => ({ id, workspace: 'acme', target });

  it('draws one row per subscription, a channel by name and an id as an id', () => {
    const rows = [
      subscription('41a2', {
        kind: 'conversation',
        id: 'C1',
        name: 'granite-staging',
        mode: 'all',
      }),
      subscription('41a3', { kind: 'dm' }),
      subscription('41a4', { kind: 'mentions' }),
      subscription('41a5', { kind: 'conversation', id: 'C9', mode: 'mentions' }),
    ];
    const card = forgeCardOf('mcp__forge__slack__subscribe', {}, result(JSON.stringify(rows)));
    expect(card?.title, 'the workspace is part of the address').toBe('subscribed in acme');
    expect(card?.chips[0], 'a channel is named, with its mode').toEqual({
      text: '#granite-staging · all',
      tone: 'info',
    });
    expect(card?.chips[1]).toEqual({ text: 'DMs', tone: 'plain' });
    expect(card?.chips[2]).toEqual({ text: 'mentions', tone: 'plain' });
    expect(card?.chips[3], 'an id is never dressed as a channel').toEqual({
      text: 'C9 · mentions',
      tone: 'info',
    });
  });

  it('names a post by its channel, with its parts and ts', () => {
    const posted = {
      ts: ['1790186552.442169', '1790186552.442170'],
      conversation_name: 'granite-staging',
      parts: 2,
    };
    const card = forgeCardOf(
      'mcp__forge__slack__post',
      { conversation: 'C1', text: 'hi', workspace: 'Trust Machines' },
      result(JSON.stringify(posted)),
    );
    expect(card?.title, 'the workspace names where it landed').toBe(
      'posted to Trust Machines · #granite-staging',
    );
    expect(card?.chips).toEqual([{ text: '2 parts', tone: 'plain' }]);
    expect(card?.figure).toBe('ts 1790186552.442169');
  });

  it('names a post by its channel alone when no workspace was stated', () => {
    const posted = { ts: ['1.0'], conversation_name: 'ops', parts: 1 };
    const card = forgeCardOf('mcp__forge__slack__post', {}, result(JSON.stringify(posted)));
    expect(card?.title).toBe('posted to #ops');
    expect(card?.chips, 'one part is not worth a word').toEqual([]);
  });

  it('names an unsubscribe by the workspace and the target that stopped', () => {
    const removed = subscription('41a2', {
      kind: 'conversation',
      id: 'C1',
      name: 'ops',
      mode: 'all',
    });
    const card = forgeCardOf(
      'mcp__forge__slack__unsubscribe',
      { id: '41a2' },
      result(JSON.stringify({ status: 'deleted', removed })),
    );
    expect(card?.title).toBe('unsubscribed in acme');
    expect(card?.chips).toEqual([{ text: '#ops · all', tone: 'dim' }]);
  });
});

describe('the review card', () => {
  it('titles a reply by the anchor the comment sits on', () => {
    const anchor = {
      comment_id: 'c-71',
      file: 'src/chat/units.ts',
      line: 919,
      side: 'new',
      status: 'addressed',
      number: 3,
    };
    const card = forgeCardOf(
      'mcp__forge__review__reply',
      { comment_id: 'c-71', text: 'It is replay-only.' },
      result(JSON.stringify(anchor)),
    );
    expect(card?.title, 'the file and line, not the comment id').toBe(
      'replied on src/chat/units.ts:919',
    );
    expect(card?.chips).toEqual([{ text: 'addressed', tone: 'info' }]);
    expect(card?.figure).toBe('c-71');
    expect(card?.pieces[0], 'and the reply itself is the body').toEqual({
      kind: 'quote',
      text: 'It is replay-only.',
    });
  });

  it('titles a resolve the same way, with the state it reached', () => {
    const anchor = {
      comment_id: 'c-68',
      file: 'src/x.rs',
      line: 48,
      side: 'new',
      status: 'resolved',
      number: 3,
    };
    const card = forgeCardOf(
      'mcp__forge__review__resolve',
      { comment_id: 'c-68' },
      result(JSON.stringify(anchor)),
    );
    expect(card?.title).toBe('resolved src/x.rs:48');
    expect(card?.chips).toEqual([{ text: 'resolved', tone: 'ok' }]);
  });

  it('tallies a review detail in its chips and lists each comment by its anchor', () => {
    const comment = (id: string, file: string, status: string, turns: number) => ({
      comment_id: id,
      file,
      line: 919,
      side: 'new',
      status,
      context: [],
      thread: Array.from({ length: turns }, () => ({})),
    });
    const detail = {
      review_id: 'r1',
      number: 3,
      summary: 'Second pass over the card grammar',
      comments: [
        comment('c-71', 'src/chat/units.ts', 'open', 0),
        comment('c-68', 'src/chat/Inbound.svelte', 'addressed', 2),
      ],
    };
    const card = forgeCardOf(
      'mcp__forge__review__get',
      { review_id: 'r1' },
      result(JSON.stringify(detail)),
    );
    expect(card?.title).toBe('review #3');
    expect(card?.chips).toEqual([
      { text: '1 open', tone: 'bad' },
      { text: '1 addressed', tone: 'info' },
    ]);
    expect(card?.figure).toBe('2 comments');
    expect(itemsOf(card)[1]).toMatchObject({
      text: 'src/chat/Inbound.svelte:919',
      state: { text: 'addressed', tone: 'info' },
      when: 'c-68 · 2 turns',
    });
  });
});

describe('the agents spawn card', () => {
  it('names the worker by the label the lead asked for, and says fresh or resumed', () => {
    const spawned = {
      session_id: 's-4b1e0000-0000-0000-0000-000000000000',
      tag: 'forge:worker:reviewer',
      resumed: false,
      session_choice: 'fresh',
      mcp_families: ['tasks', 'slack'],
      worktree: '/repo/.claude/worktrees/reviewer',
    };
    const card = forgeCardOf(
      'mcp__forge__agents__spawn',
      { label: 'reviewer', charter: 'review the diff', kick: 'start with the diff' },
      result(JSON.stringify(spawned)),
    );
    expect(card?.title).toBe("spawned worker 'reviewer'");
    expect(card?.chips).toEqual([
      { text: 'fresh', tone: 'plain' },
      { text: 'tasks, slack', tone: 'dim' },
    ]);
    expect(card?.figure, 'the session it landed on, short').toBe('s-4b1e');
    expect(pairsOf(card), 'where it landed').toEqual([
      ['worktree', '/repo/.claude/worktrees/reviewer'],
    ]);
    expect(card?.pieces.at(-1), 'and the kick it was started with').toEqual({
      kind: 'quote',
      text: 'start with the diff',
    });
  });

  it('says resumed when the label came back', () => {
    const spawned = { session_id: 's-77aa', resumed: true, session_choice: 'resumed' };
    const card = forgeCardOf(
      'mcp__forge__agents__spawn',
      { label: 'implementer', charter: 'c' },
      result(JSON.stringify(spawned)),
    );
    expect(card?.chips[0]).toEqual({ text: 'resumed', tone: 'info' });
    expect(card?.chips[1], 'no families stated means every family').toEqual({
      text: 'all families',
      tone: 'dim',
    });
  });
});
