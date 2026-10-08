// @vitest-environment jsdom
import { flushSync, mount, unmount } from 'svelte';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { homeWire } from '../dev/fixture.data';
import type { BoardRow, HomeWire, Marks, Task, TaskStatus } from '../wire/home';
import Board from './Board.svelte';

/**
 * The board's controls: every press hands the app one command with its
 * project on it, and the top bar's back and done close the takeover.
 *
 * The page is a view - it builds the commands and hands them out - so what
 * this pins is the command each control produces, not what the core does
 * with it.
 */
let app: Record<string, unknown> | null = null;
let acts: Record<string, Record<string, unknown>>[] = [];

afterEach(async () => {
  if (app !== null) await unmount(app);
  app = null;
  document.body.innerHTML = '';
});

beforeEach(() => {
  acts = [];
});

function marks(over: Partial<Marks> = {}): Marks {
  return {
    ready: false,
    in_review: false,
    overdue: false,
    no_movement: false,
    waiting_too_long: false,
    stale: false,
    to_close: false,
    ...over,
  };
}

function task(id: string, subject: string, status: TaskStatus, over: Partial<Task> = {}): Task {
  return {
    id,
    project_name: 'proj',
    subject,
    active_form: null,
    detail: null,
    status,
    owner: null,
    parent: null,
    waiting_on: null,
    estimate: null,
    rank: null,
    verify: null,
    links: [],
    attempt: 0,
    archived_at: null,
    created_at: { secs_since_epoch: 0, nanos_since_epoch: 0 },
    updated_at: { secs_since_epoch: 0, nanos_since_epoch: 0 },
    ...over,
  };
}

function row(task: Task, over: Partial<BoardRow> = {}): BoardRow {
  return {
    task,
    worked_secs: 0,
    updated_secs_ago: 0,
    marks: marks(),
    rollup: null,
    parent_subject: null,
    ...over,
  };
}

function wireWith(rows: BoardRow[]): HomeWire {
  const first = homeWire.projects[0];
  if (first === undefined) throw new Error('the fixture holds no project');
  return { ...homeWire, projects: [{ ...first, rows }] };
}

function summon(wire: HomeWire): void {
  app = mount(Board, {
    target: document.body,
    props: {
      wire,
      org: 'TestOrg',
      project: 'proj',
      onact: (command: Record<string, Record<string, unknown>>) => acts.push(command),
    },
  });
  flushSync();
}

const byText = (text: string): HTMLElement | null => {
  const found = [...document.querySelectorAll<HTMLElement>('button')].find(
    (button) => button.textContent?.trim() === text,
  );
  return found ?? null;
};

describe("the board's controls", () => {
  it('hands approve, send-back and answer out as commands with their project', () => {
    const waiting = (id: string, verification: boolean) =>
      row(
        task(id, `subject ${id}`, 'waiting', {
          waiting_on: { kind: 'decision', detail: 'the words', on: null, verification },
        }),
      );
    summon(wireWith([waiting('v1', true), waiting('q1', false)]));

    byText('approve')?.click();
    flushSync();
    expect(acts[0]).toEqual({ task_verdict: { project: 'proj', id: 'v1', approve: true } });

    // The send-back carries whatever the reader typed into its own box.
    const words = document.querySelector<HTMLInputElement>(
      'input[placeholder="what to change (sent back with it)"]',
    );
    if (words === null) throw new Error('the send-back box is not on the page');
    words.value = 'the rail elides wrong';
    words.dispatchEvent(new Event('input'));
    flushSync();
    byText('send back')?.click();
    flushSync();
    expect(acts[1]).toEqual({
      task_verdict: {
        project: 'proj',
        id: 'v1',
        approve: false,
        words: 'the rail elides wrong',
      },
    });

    const answer = document.querySelector<HTMLInputElement>('input[placeholder="your answer"]');
    if (answer === null) throw new Error('the answer box is not on the page');
    answer.value = 'keep the window raise';
    answer.dispatchEvent(new Event('input'));
    flushSync();
    byText('answer')?.click();
    flushSync();
    expect(acts[2]).toEqual({
      task_answer: { project: 'proj', id: 'q1', words: 'keep the window raise' },
    });
  });

  it('hands rank moves and assignments out with the row they move', () => {
    summon(
      wireWith([
        row(task('t1', 'a row', 'pending')),
        row(task('t2', 'another', 'pending', { rank: 1 })),
      ]),
    );

    // The board draws the queue's order, so t2 (rank 1) leads and its
    // controls are the ones a reader meets first.
    document.querySelector<HTMLElement>('button[title="move to the top"]')?.click();
    flushSync();
    expect(acts[0]).toEqual({ task_rank: { project: 'proj', id: 't2', to: 'top' } });

    const pick = document.querySelector<HTMLSelectElement>(
      'select[aria-label="the seat holding another"]',
    );
    if (pick === null) throw new Error('the owner picker is not on the page');
    pick.value = 'lead';
    // Bubbling, because Svelte delegates `change` at the root.
    pick.dispatchEvent(new Event('change', { bubbles: true }));
    flushSync();
    expect(acts[1]).toEqual({ task_assign: { project: 'proj', id: 't2', owner: 'lead' } });
  });

  it('cuts a new row with its subject and epic', () => {
    summon(wireWith([row(task('epic', 'the epic', 'pending'))]));

    const subject = document.querySelector<HTMLInputElement>('input[placeholder="a new row"]');
    if (subject === null) throw new Error('the create box is not on the page');
    subject.value = 'a fresh row';
    subject.dispatchEvent(new Event('input'));
    flushSync();
    const parent = document.querySelector<HTMLSelectElement>(
      'select[aria-label="the epic it belongs under"]',
    );
    if (parent === null) throw new Error('the epic picker is not on the page');
    parent.value = 'epic';
    parent.dispatchEvent(new Event('change'));
    flushSync();
    byText('+ add')?.click();
    flushSync();
    expect(acts[0]).toEqual({
      task_create: { project: 'proj', subject: 'a fresh row', parent: 'epic' },
    });
  });

  it('closes the takeover through history from back and done alike', () => {
    // The reader arrived by a push (a fleet row or the tasks strip), so
    // there is a page to come back to; a deep link falls to the home.
    history.pushState({}, '', '/board/TestOrg/proj');
    const back = vi.spyOn(window.history, 'back').mockImplementation(() => {});
    summon(wireWith([]));

    const backButton = [...document.querySelectorAll<HTMLElement>('button')].find((button) =>
      button.textContent?.includes('back'),
    );
    if (backButton === undefined) throw new Error('the back control is not on the page');
    backButton.click();
    byText('done')?.click();
    flushSync();
    expect(back).toHaveBeenCalledTimes(2);
    back.mockRestore();
  });

  /** Every state draws, one class per status - shape and colour both. */
  it('draws a mark per status', () => {
    summon(
      wireWith(
        (['pending', 'in_progress', 'waiting', 'completed', 'failed', 'canceled'] as const).map(
          (status) => row(task(status, `a ${status} row`, status)),
        ),
      ),
    );

    for (const cls of ['b-pend', 'b-run', 'b-wait', 'b-done', 'b-fail', 'b-cancel']) {
      expect(document.querySelector(`.b-mark.${cls}`), `${cls} is drawn`).not.toBeNull();
    }
  });
});
