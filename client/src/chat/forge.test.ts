import { describe, expect, it } from 'vitest';

import { forgeCardOf, forgeRowTitle, type ForgeCard } from './forge';

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
function itemsOf(card: ForgeCard | null) {
  const list = (card?.pieces ?? []).find((piece) => piece.kind === 'list');
  return list?.kind === 'list' ? list.items : [];
}

/** The blocks of a card's first comments piece, or an empty list. */
function commentsOf(card: ForgeCard | null) {
  const block = (card?.pieces ?? []).find((piece) => piece.kind === 'comments');
  return block?.kind === 'comments' ? block.items : [];
}

describe('a forge call with no card of its own', () => {
  it('is titled by the subject its input named, else by the family own noun', () => {
    expect(
      forgeRowTitle('mcp__forge__slack__list', {}, 'failed'),
      'a read with no input to name a subject by',
    ).toBe('conversations');
    expect(
      forgeRowTitle('mcp__forge__tasks__create', { subject: 'Sweep the worktrees' }, 'failed'),
      'a create names what it was making',
    ).toBe('Sweep the worktrees');
    expect(
      forgeRowTitle('mcp__forge__agents__spawn', { label: 'reviewer' }, 'failed'),
      'a failed spawn uses the success wording, under a tail that says it failed',
    ).toBe("spawned worker 'reviewer'");
    expect(
      forgeRowTitle('mcp__forge__agents__spawn', { label: 'reviewer' }, 'running'),
      'while a spawn still out reads in flight',
    ).toBe('spawning reviewer');
    expect(forgeRowTitle('mcp__forge__slack__post', { conversation: 'C1' }, 'failed')).toBe(
      'posted to C1',
    );
    expect(forgeRowTitle('mcp__forge__slack__search', { query: 'smoke test' }, 'failed')).toBe(
      'search \u{b7} smoke test',
    );
    expect(forgeRowTitle('Bash', {}, 'failed'), 'and no other call is titled here').toBeNull();
  });
});

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

  it('refuses a payload each family reader cannot name its subject by', () => {
    // One case per reader the arms share, so a relaxed guard is a failed
    // assertion rather than a card drawing under a name nobody has.
    const named = (name: string, answer: unknown): ForgeCard | null =>
      forgeCardOf(name, {}, result(JSON.stringify(answer)));

    expect(named('mcp__forge__gotify__list', [{ applications: ['Backups'] }]), 'no id').toBeNull();
    expect(
      named('mcp__forge__gotify__unsubscribe', { status: 'deleted', removed: {} }),
      'a removed subscription the echo does not name',
    ).toBeNull();
    expect(
      named('mcp__forge__cron__list', [{ description: 'Morning summary' }]),
      'no id',
    ).toBeNull();
    expect(named('mcp__forge__tasks__list', [{ id: 't-1' }]), 'no subject').toBeNull();
    expect(
      named('mcp__forge__slack__unsubscribe', {
        status: 'deleted',
        removed: { target: { kind: 'conversation' } },
      }),
      'a target with no id',
    ).toBeNull();
    expect(
      named('mcp__forge__slack__post', { ts: '1790186552.442169' }),
      'a ts that is not a list',
    ).toBeNull();
    expect(
      named('mcp__forge__agents__capacity', { cap: '8', live: 7 }),
      'numbers arriving as strings',
    ).toBeNull();

    // The schedule reader has no null: a schedule it cannot read draws as the
    // one word rather than dropping the entry.
    const unreadable = named('mcp__forge__cron__list', [
      { id: 'c1', prompt: 'sweep', schedule: {} },
    ]);
    expect(unreadable?.chips, 'an unreadable schedule still draws a word').toEqual([]);
    expect(itemsOf(unreadable)[0]?.tag).toBe('scheduled');
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

  it('draws a project with no schedules and no subscriptions as words too', () => {
    expect(forgeCardOf('mcp__forge__cron__list', {}, result('[]'))?.pieces).toEqual([
      { kind: 'empty', text: 'no schedules registered by this session' },
    ]);
    expect(forgeCardOf('mcp__forge__gotify__list', {}, result('[]'))?.pieces).toEqual([
      {
        kind: 'empty',
        text: 'no Gotify subscriptions - nothing from the server reaches this session',
      },
    ]);
  });

  it("lists a project's tasks as rows, each naming its own state", () => {
    const rows = [
      RECORD,
      { ...RECORD, id: 't-2', subject: 'Sweep', status: 'pending', owner: null, estimate: null },
    ];
    const card = forgeCardOf('mcp__forge__tasks__list', {}, result(JSON.stringify(rows)));
    expect(card?.title).toBe('tasks');
    expect(card?.figure).toBe('2 in flight');
    expect(itemsOf(card)).toEqual([
      {
        id: 't-1',
        state: { text: 'in progress', tone: 'info' },
        text: RECORD.subject,
        tag: null,
        when: 'lead \u{b7} 2h',
      },
      {
        id: 't-2',
        state: { text: 'pending', tone: 'dim' },
        text: 'Sweep',
        tag: null,
        when: 'unclaimed',
      },
    ]);
  });

  it('draws a list of nothing as a row, not as an empty box', () => {
    const card = forgeCardOf('mcp__forge__tasks__list', {}, result('[]'));
    expect(card?.title).toBe('tasks');
    expect(card?.figure, 'no count for no rows').toBeNull();
    expect(card?.pieces, 'what it found nothing of, in words').toEqual([
      { kind: 'empty', text: 'no tasks in flight - anything this project declares lands here' },
    ]);
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
      ['lands in', 'this session'],
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
    expect(card?.chips, 'the whole filter that stopped, as one record').toEqual([
      { text: 'Backups, Alerts \u{b7} priority \u{2265} 5', tone: 'dim' },
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

  it('cards a write from its input, whose result is only one word', () => {
    // `slack__edit` and `slack__react` answer with a bare acknowledgement, so
    // the subject comes from the input: the message's id, and the shortcode.
    const edited = forgeCardOf(
      'mcp__forge__slack__edit',
      { conversation: 'C1', ts: '179.1', text: 'Deploy is green now.' },
      result('updated'),
    );
    expect(edited?.title).toBe('updated a message in C1');
    expect(edited?.pieces).toEqual([{ kind: 'quote', text: 'Deploy is green now.' }]);

    const deleted = forgeCardOf(
      'mcp__forge__slack__edit',
      { conversation: 'C1', ts: '179.1', delete: true },
      result('deleted'),
    );
    expect(deleted?.title).toBe('deleted a message in C1');
    expect(deleted?.pieces, 'a deletion has no replacement body to draw').toEqual([]);

    const reacted = forgeCardOf(
      'mcp__forge__slack__react',
      { conversation: 'C1', ts: '179.1', name: 'white_check_mark' },
      result('reaction applied'),
    );
    expect(reacted?.title).toBe('reacted :white_check_mark: in C1');

    const removed = forgeCardOf(
      'mcp__forge__slack__react',
      { conversation: 'C1', ts: '179.1', name: 'x', remove: true },
      result('reaction applied'),
    );
    expect(removed?.title, 'removing is not reacting').toBe('removed :x: in C1');
  });

  it('lists pins by their text and bookmarks by their title, either fallback to the link', () => {
    const pins = forgeCardOf(
      'mcp__forge__slack__pins',
      { conversation: 'C1', workspace: 'acme' },
      result(JSON.stringify([{ ts: '1.0', user: 'U1', text: 'standup at 9' }])),
    );
    expect(pins?.title).toBe('pins in C1 \u{b7} acme');
    expect(itemsOf(pins)[0]).toMatchObject({ text: 'standup at 9', when: 'U1' });

    const bookmarks = forgeCardOf(
      'mcp__forge__slack__bookmarks',
      { conversation: 'C1', workspace: 'acme' },
      result(JSON.stringify([{ id: 'B1', title: null, link: 'https://runbook' }])),
    );
    expect(bookmarks?.title).toBe('bookmarks in C1 \u{b7} acme');
    expect(
      itemsOf(bookmarks)[0],
      'an untitled bookmark draws the link as its own words',
    ).toMatchObject({ text: 'https://runbook' });
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

  it('tallies a review detail, drawing each comment as its own block', () => {
    const comment = (id: string, file: string, status: string, turns: number) => ({
      comment_id: id,
      file,
      line: 919,
      side: 'new',
      status,
      context: ['mcp__forge__agents__send_message: { body: "message" },'],
      thread: Array.from({ length: turns }, (_held, at) => ({
        author: at === 0 ? 'you' : 'worker',
        text: at === 0 ? 'Reading this fresh' : 'Right - that is shipped',
        at: '2026-10-05T09:00:00Z',
        review: null,
      })),
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
    expect(card?.title, 'the tally is in the title, the way the mock draws it').toBe(
      'review #3 - 1 open, 1 addressed',
    );
    expect(card?.figure).toBe('2 comments');
    const blocks = commentsOf(card);
    expect(blocks[1], 'each comment is a block of its own').toMatchObject({
      where: 'src/chat/Inbound.svelte:919',
      side: 'new side',
      state: { text: 'addressed', tone: 'info' },
      context: ['mcp__forge__agents__send_message: { body: "message" },'],
    });
    expect(blocks[1]?.turns, 'with the thread it was argued in').toEqual([
      { author: 'you', text: 'Reading this fresh', you: true },
      { author: 'worker', text: 'Right - that is shipped', you: false },
    ]);
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

describe('the reads that draw a list or a fact', () => {
  it('lists the Gotify applications the server knows', () => {
    const card = forgeCardOf(
      'mcp__forge__gotify__apps',
      {},
      result(JSON.stringify(['Backups', 'Alerts'])),
    );
    expect(card?.title).toBe('applications');
    expect(card?.figure).toBe('2');
    expect(itemsOf(card).map((item) => item.text)).toEqual(['Backups', 'Alerts']);
  });

  it('draws a Slack search as its hits, the query in the title', () => {
    const card = forgeCardOf(
      'mcp__forge__slack__search',
      { query: 'smoke test', workspace: 'acme' },
      result(
        JSON.stringify([
          {
            ts: '1.0',
            text: 'smoke test passed on 1.0.115',
            conversation: 'C1',
            conversation_name: 'deploys',
            username: 'bot',
          },
        ]),
      ),
    );
    expect(card?.title, 'the query is the subject, the workspace follows').toBe(
      'search \u{b7} "smoke test" \u{b7} acme',
    );
    expect(card?.figure).toBe('1 hits');
    expect(itemsOf(card)[0]).toMatchObject({
      text: 'smoke test passed on 1.0.115',
      tag: '#deploys',
      when: 'bot',
    });
  });

  it('lists Slack conversations, marking the ones this session watches', () => {
    const card = forgeCardOf(
      'mcp__forge__slack__list',
      { workspace: 'acme' },
      result(
        JSON.stringify({
          conversations: [
            { id: 'C1', name: 'ops', kind: 'public', subscribed: true, subscription_ids: ['s1'] },
            { id: 'C2', name: 'general', kind: 'public', subscribed: false, subscription_ids: [] },
          ],
          subscriptions: [],
        }),
      ),
    );
    expect(card?.title).toBe('conversations \u{b7} acme');
    expect(card?.figure).toBe('2');
    expect(itemsOf(card)[0]).toMatchObject({
      text: '#ops',
      tag: 'public',
      state: { text: 'watching', tone: 'ok' },
    });
    expect(itemsOf(card)[1]?.state, 'an unwatched conversation carries no mark').toBeNull();
  });

  it('draws a Slack user as its own facts', () => {
    const card = forgeCardOf(
      'mcp__forge__slack__user',
      { user: 'U1' },
      result(JSON.stringify({ id: 'U1', name: 'alex', real_name: 'Alex Doe', tz: 'Asia/Kolkata' })),
    );
    expect(card?.title).toBe('user alex');
    expect(pairsOf(card)).toEqual([
      ['real name', 'Alex Doe'],
      ['tz', 'Asia/Kolkata'],
    ]);
    expect(card?.figure, 'a user is not a count').toBeNull();
  });

  it('draws the newest review by its number, tally and date, older rounds under it', () => {
    const review = (number: number, summary: string, open: number, addressed: number) => ({
      review_id: `rv-${String(number)}`,
      number,
      summary,
      created_at: '2026-10-05T09:00:00Z',
      comment_count: open + addressed,
      open,
      addressed,
      resolved: 0,
      outdated: 0,
    });

    const card = forgeCardOf(
      'mcp__forge__review__list',
      {},
      result(
        JSON.stringify([
          review(3, 'Second pass over the card grammar', 4, 3),
          review(2, 'First pass', 0, 0),
        ]),
      ),
    );
    expect(card?.title, 'the newest, by number and summary').toBe(
      'review #3 - Second pass over the card grammar',
    );
    expect(card?.chips, 'its tally as chips').toEqual([
      { text: '4 open', tone: 'bad' },
      { text: '3 addressed', tone: 'info' },
    ]);
    expect(card?.figure).toMatch(/^[a-z]{3} \d{1,2}$/);
    expect(itemsOf(card)[0], 'and the rounds under it').toMatchObject({
      text: 'First pass',
      tag: '#2',
    });

    const none = forgeCardOf('mcp__forge__review__list', {}, result('[]'));
    expect(none?.pieces).toEqual([{ kind: 'empty', text: 'no reviews on this branch' }]);
  });
});

describe('the worker lifecycle cards', () => {
  it('names an update by its label and chips the fields the server wrote', () => {
    // The result carries the field NAMES, not their new values, so the row
    // says what moved and never what it moved to.
    const card = forgeCardOf(
      'mcp__forge__agents__update',
      { label: 'card-smoke', kick: 'stand down' },
      result(JSON.stringify({ label: 'card-smoke', updated: ['kick', 'resume_kick'] })),
    );
    expect(card?.title).toBe("updated worker 'card-smoke'");
    expect(card?.chips, 'the field names as the server spells them').toEqual([
      { text: 'kick', tone: 'plain' },
      { text: 'resume_kick', tone: 'plain' },
    ]);
    expect(card?.figure, 'when the fields take effect').toBe('next respawn');
  });

  it('draws capacity as its two numbers and the proportion between them', () => {
    const card = forgeCardOf(
      'mcp__forge__agents__capacity',
      {},
      result(
        JSON.stringify({
          project: 'forge',
          cap: 8,
          live: 7,
          available: 1,
          cap_source: 'max_workers',
        }),
      ),
    );
    expect(card?.title).toBe('worker capacity');
    expect(card?.chips, 'both numbers, each in its own words').toEqual([
      { text: '7 live', tone: 'plain' },
      { text: 'cap 8 \u{b7} forge.toml', tone: 'dim' },
    ]);
    expect(card?.figure).toBe('1 free');
    expect(card?.meter, 'the bar is the chips drawn, never their substitute').toEqual({
      fill: 7,
      of: 8,
    });
    expect(pairsOf(card), 'where the default came from, as the server names it').toEqual([
      ['project', 'forge'],
      ['cap source', 'max_workers'],
    ]);
  });

  it('names a despawn by the worker it closed', () => {
    const card = forgeCardOf(
      'mcp__forge__agents__despawn',
      { label: 'card-smoke' },
      result(JSON.stringify({ status: 'despawned' })),
    );
    expect(card?.title).toBe("closed worker 'card-smoke'");
    expect(card?.chips).toEqual([{ text: 'worktree removed', tone: 'dim' }]);
    expect(card?.pieces, 'a clean despawn carries no text').toEqual([]);
  });

  it('keeps what a despawn could not clean as its warnings', () => {
    const card = forgeCardOf(
      'mcp__forge__agents__despawn',
      { label: 'card-smoke' },
      result(
        JSON.stringify({
          status: 'despawned',
          branch_cleanup_warning: "branch 'worktree-w1' kept: 2 commits",
        }),
      ),
    );
    expect(card?.title).toBe("closed worker 'card-smoke'");
    expect(card?.pieces).toEqual([
      { kind: 'warnline', label: 'branch', text: "branch 'worktree-w1' kept: 2 commits" },
    ]);
  });

  it('draws a call still out from its own input, saying the wait it is in', () => {
    // The dock holds a Slack write until the user answers, and an approval
    // prompt nobody has answered is the one thing the row must not hide.
    const posting = forgeCardOf(
      'mcp__forge__slack__post',
      { conversation: 'C1', workspace: 'Trust Machines', text: 'Deploy is green.' },
      undefined,
    );
    expect(posting?.title, 'where it would land, from the id alone').toBe(
      'posting to Trust Machines \u{b7} C1',
    );
    expect(posting?.chips).toEqual([{ text: 'waiting for your approval', tone: 'warn' }]);
    expect(posting?.pieces, 'and the draft, which is what is being asked about').toEqual([
      { kind: 'quote', text: 'Deploy is green.' },
    ]);

    const spawning = forgeCardOf(
      'mcp__forge__agents__spawn',
      { label: 'reviewer', charter: 'be terse' },
      undefined,
    );
    expect(spawning?.title).toBe('spawning reviewer');
    expect(spawning?.pieces.at(-1)).toEqual({ kind: 'quote', text: 'be terse' });

    expect(
      forgeCardOf('mcp__forge__tasks__list', {}, undefined),
      'a verb with nothing to say before it answers',
    ).toBeNull();
    const creating = forgeCardOf(
      'mcp__forge__tasks__create',
      { subject: 'Draft the cron card copy', status: 'pending' },
      undefined,
    );
    expect(creating?.title, 'a call still out says what it is making').toBe(
      'Draft the cron card copy',
    );
    expect(creating?.chips, 'and the state it is making it in').toEqual([
      { text: 'pending', tone: 'dim' },
    ]);
  });

  it('says a refused despawn on the row, since the call itself answered cleanly', () => {
    // The blocked shape is a RESULT, not a tool error: nothing failed, and the
    // reason is still the one thing a reader acts on.
    const card = forgeCardOf(
      'mcp__forge__agents__despawn',
      { label: 'implementer' },
      result(JSON.stringify({ status: 'blocked', reason: '3 uncommitted files' })),
    );
    expect(card?.title).toBe("worker 'implementer' still live");
    expect(card?.tail, 'the reason rides the row').toEqual({
      text: '3 uncommitted files',
      tone: 'warn',
    });
    expect(card?.figure, 'no figures for a refusal').toBeNull();
    expect(card?.pieces[0], 'and the body keeps it in words').toMatchObject({
      kind: 'warnline',
      label: 'blocked',
    });
  });
});
