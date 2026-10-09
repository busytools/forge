// @vitest-environment jsdom
import { flushSync, mount, unmount } from 'svelte';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { homeWire } from '../dev/fixture.data';
import { violationsOf } from '../testing/axe';
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

function summon(
  wire: HomeWire,
  onact: (command: Record<string, Record<string, unknown>>) => void = (command) =>
    acts.push(command),
): void {
  app = mount(Board, {
    target: document.body,
    props: { wire, org: 'TestOrg', project: 'proj', onact },
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

    // The board draws the queue's order, so t2 (rank 1) leads. There are no
    // rank buttons: a focused card's arrow keys are the keyboard's half of
    // the drag, so the arrow on the leading card is where a move comes from.
    const first = document.querySelector<HTMLElement>(
      '.b-lane[data-state="pending"] .b-card[data-id="t2"]',
    );
    if (first === null) throw new Error('the leading card is not on the page');
    first.dispatchEvent(new KeyboardEvent('keydown', { key: 'ArrowDown', bubbles: true }));
    flushSync();
    expect(acts[0]).toEqual({ task_rank: { project: 'proj', id: 't2', to: 'down' } });

    // The owner picker is the board's own: the button opens the list, and
    // the list's item is what assigns.
    const pick = document.querySelector<HTMLElement>(
      'button[aria-label="the seat holding another"]',
    );
    if (pick === null) throw new Error('the owner picker is not on the page');
    pick.click();
    flushSync();
    const item = [...document.querySelectorAll<HTMLElement>('.b-menu .b-item')].find(
      (entry) => entry.textContent?.trim() === 'lead',
    );
    if (item === undefined) throw new Error('the seat is not in the open list');
    item.click();
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
    const parent = document.querySelector<HTMLElement>(
      'button[aria-label="the epic it belongs under"]',
    );
    if (parent === null) throw new Error('the epic picker is not on the page');
    parent.click();
    flushSync();
    const item = [...document.querySelectorAll<HTMLElement>('.b-menu .b-item')].find(
      (entry) => entry.textContent?.trim() === 'the epic',
    );
    if (item === undefined) throw new Error('the epic is not in the open list');
    item.click();
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

  /**
   * Drag and drop is the board's own interaction: a drop on another lane
   * is a move, a drop in the card's own lane is a rank, and a seat takes
   * it as an assignment. The arrows on a focused card are the keyboard's.
   */
  it('drags a card to a lane, within a lane, and onto a seat', () => {
    const lead = { org: 'TestOrg', project: 'proj', label: 'lead' };
    summon(
      wireWith([
        row(task('t1', 'a row', 'pending', { rank: 1 })),
        row(task('t2', 'another', 'pending', { owner: lead })),
      ]),
    );

    // jsdom performs no layout and carries no elementFromPoint, so the hit
    // test is stubbed: the point resolves to whichever element the case is
    // about. The descriptor comes back out afterwards.
    const original = Object.getOwnPropertyDescriptor(document, 'elementFromPoint');
    document.elementFromPoint = () => null;

    const drag = (onto: () => Element | null): void => {
      // Found per drag: the card is re-rendered in its new lane between
      // cases, so a held element would be the detached one.
      const card = document.querySelector<HTMLElement>('.b-card[data-id="t2"]');
      if (card === null) throw new Error('the card is not on the page');
      card.dispatchEvent(
        new MouseEvent('pointerdown', { bubbles: true, clientX: 10, clientY: 10, button: 0 }),
      );
      document.elementFromPoint = onto;
      window.dispatchEvent(new MouseEvent('pointermove', { clientX: 60, clientY: 60 }));
      window.dispatchEvent(new MouseEvent('pointerup', {}));
      flushSync();
    };

    // Onto the Waiting lane: the row moves state, and the card lands in
    // that lane at once - before any wire snapshot could arrive.
    drag(() => document.querySelector('.b-lane[data-state="waiting"]'));
    expect(acts[0]).toEqual({ task_move: { project: 'proj', id: 't2', to: 'waiting' } });
    expect(
      document.querySelector('.b-lane[data-state="waiting"] .b-card[data-id="t2"]'),
      'the card is in its new lane the moment the pointer lets go',
    ).not.toBeNull();

    // Back in its own lane: the drop position is a rank (the end, here:
    // jsdom's zeroed rects put no card above the point).
    drag(() => document.querySelector('.b-lane[data-state="pending"]'));
    expect(acts[1]).toEqual({ task_rank: { project: 'proj', id: 't2', to: { before: null } } });

    // Onto the lead's seat: the row is assigned to it.
    drag(() => document.querySelector('[data-seat="lead"]'));
    expect(acts[2]).toEqual({ task_assign: { project: 'proj', id: 't2', owner: 'lead' } });

    if (original !== undefined) {
      Object.defineProperty(document, 'elementFromPoint', original);
    } else {
      Reflect.deleteProperty(document, 'elementFromPoint');
    }
  });

  /**
   * The keyboard has the whole drag, not half of it. The arrows re-order a
   * card within its lane, and left and right move it to the neighbouring lane
   * in the order the lanes are drawn - the board's headline action, which the
   * pointer and the keys have to reach alike.
   */
  it('moves a focused card between lanes with the arrows', () => {
    summon(wireWith([row(task('t1', 'a row', 'pending'))]));
    const press = (key: string): void => {
      // Re-queried per press: the card is drawn in its new lane between them.
      const card = document.querySelector<HTMLElement>('.b-card[data-id="t1"]');
      if (card === null) throw new Error('the card is not on the page');
      card.dispatchEvent(new KeyboardEvent('keydown', { key, bubbles: true }));
      flushSync();
    };

    // Ready's neighbour to the right is Completed, and to the left Waiting.
    press('ArrowRight');
    expect(acts[0]).toEqual({ task_move: { project: 'proj', id: 't1', to: 'completed' } });
    expect(
      document.querySelector('.b-lane[data-state="completed"] .b-card[data-id="t1"]'),
      'the card is in the lane the key sent it to',
    ).not.toBeNull();
    press('ArrowLeft');
    expect(acts[1]).toEqual({ task_move: { project: 'proj', id: 't1', to: 'waiting' } });
  });

  /**
   * A key that arrives from a control inside the card belongs to that control:
   * the owner picker's own arrows walk its menu, and must not also re-order
   * the row underneath them.
   */
  it('leaves the keys inside a card to the control that holds them', () => {
    summon(wireWith([row(task('t1', 'a row', 'pending', { rank: 1 }))]));
    const pick = document.querySelector<HTMLElement>('button[aria-label="the seat holding a row"]');
    if (pick === null) throw new Error('the owner picker is not on the page');
    pick.click();
    flushSync();

    pick.dispatchEvent(new KeyboardEvent('keydown', { key: 'ArrowDown', bubbles: true }));
    flushSync();
    expect(acts, 'the picker key reached the board as a re-order').toEqual([]);
  });

  /**
   * A press on a card focuses it. `preventDefault` on the press is what stops
   * the text selection, and it takes the focus a press would have given with
   * it - so without this the arrows would only ever reach a card that Tab
   * reached.
   */
  it('focuses the card a press lands on', () => {
    summon(wireWith([row(task('t1', 'a row', 'pending'))]));
    const card = document.querySelector<HTMLElement>('.b-card[data-id="t1"]');
    if (card === null) throw new Error('the card is not on the page');
    card.dispatchEvent(
      new MouseEvent('pointerdown', { bubbles: true, button: 0, clientX: 5, clientY: 5 }),
    );
    flushSync();
    expect(document.activeElement, 'the press left the focus elsewhere').toBe(card);
  });

  /**
   * An edit the socket will not carry says so, and the card does not move:
   * `dispatch` throws synchronously on a closed socket, and an optimistic
   * placement laid over that throw would leave the board drawing a lane the
   * core never accepted.
   */
  it('says so when the socket will not carry an edit, and moves nothing', () => {
    summon(wireWith([row(task('t1', 'a row', 'pending'))]), () => {
      throw new Error('the socket is not open');
    });

    const original = Object.getOwnPropertyDescriptor(document, 'elementFromPoint');
    document.elementFromPoint = () => document.querySelector('.b-lane[data-state="waiting"]');
    const card = document.querySelector<HTMLElement>('.b-card[data-id="t1"]');
    if (card === null) throw new Error('the card is not on the page');
    card.dispatchEvent(
      new MouseEvent('pointerdown', { bubbles: true, clientX: 10, clientY: 10, button: 0 }),
    );
    window.dispatchEvent(new MouseEvent('pointermove', { clientX: 60, clientY: 60 }));
    window.dispatchEvent(new MouseEvent('pointerup', {}));
    flushSync();

    expect(document.querySelector('.b-said')?.textContent, 'the loss went unsaid').toContain(
      'the socket is closed',
    );
    expect(
      document.querySelector('.b-lane[data-state="waiting"] .b-card[data-id="t1"]'),
      'the card was placed in a lane the core never accepted',
    ).toBeNull();

    if (original !== undefined) {
      Object.defineProperty(document, 'elementFromPoint', original);
    } else {
      Reflect.deleteProperty(document, 'elementFromPoint');
    }
  });

  /**
   * The core's own words for a refused edit draw on the page: a press that
   * did nothing and a press the core refused must not read the same.
   */
  it("draws the core's refusal where the reader is looking", () => {
    app = mount(Board, {
      target: document.body,
      props: {
        wire: wireWith([row(task('t1', 'a row', 'pending'))]),
        org: 'TestOrg',
        project: 'proj',
        onact: () => {},
        notice: { severity: 'warning' as const, message: 'The board refused a move: nope' },
      },
    });
    flushSync();
    expect(document.querySelector('.b-said.warning')?.textContent).toBe(
      'The board refused a move: nope',
    );
  });

  /**
   * And ONLY that edit's words. The service line carries every producer's
   * report, so a page that drew the last one would put a worker's failed spawn
   * above the lanes, where it reads as something about the board.
   */
  it("draws none of another producer's report", () => {
    app = mount(Board, {
      target: document.body,
      props: {
        wire: wireWith([row(task('t1', 'a row', 'pending'))]),
        org: 'TestOrg',
        project: 'proj',
        onact: () => {},
        notice: {
          severity: 'error' as const,
          message: 'could not spawn a worker: the directory is gone',
        },
      },
    });
    flushSync();
    expect(document.querySelector('.b-said'), 'a foreign report drew on the board').toBeNull();
  });

  /**
   * A project the fleet does not carry draws as unknown: no lanes to move a
   * card between and no create line, because a row filed here would name a
   * project this forge does not hold. The route is addressable, so the page
   * has to answer for the address rather than invite work against nothing.
   */
  it('draws an unknown project as unknown, and offers it no create line', () => {
    app = mount(Board, {
      target: document.body,
      props: {
        wire: homeWire,
        org: 'TestOrg',
        project: 'nope',
        onact: () => {},
      },
    });
    flushSync();

    expect(document.body.textContent, 'the page did not say the project is unknown').toContain(
      'not one of this forge',
    );
    expect(
      document.querySelector('.b-create'),
      'an unknown project offered a create line',
    ).toBeNull();
    expect(document.querySelector('.b-lanes'), 'an unknown project drew lanes').toBeNull();
  });

  /**
   * The picker's OPEN menu, which no server render reaches: the board draws
   * it closed, so its listbox, its options and the button's expanded state
   * would go unguarded by the page's own axe pass - and a menu is exactly
   * where a role or a name goes missing unnoticed.
   */
  it('draws the open owner picker with no violations', async () => {
    summon(wireWith([row(task('t1', 'a row', 'pending'))]));
    const pick = document.querySelector<HTMLElement>('button[aria-label="the seat holding a row"]');
    if (pick === null) throw new Error('the owner picker is not on the page');
    pick.click();
    flushSync();
    expect(document.querySelector('.b-menu'), 'the menu did not open').not.toBeNull();

    expect(await violationsOf(document.body.innerHTML)).toEqual([]);
  });

  /**
   * **A press the core would refuse is not offered.** A root cannot complete
   * while any child is open, so the strip draws what still has to close and
   * withholds the approve - the send-back stays, because the core takes it.
   */
  it('holds the approve back while a root still has open children', () => {
    summon(
      wireWith([
        row(
          task('epic', 'the epic', 'waiting', {
            waiting_on: { kind: 'decision', detail: null, on: null, verification: true },
          }),
          { rollup: [2, 4] },
        ),
      ]),
    );

    expect(document.body.textContent, 'the strip did not say what holds it').toContain(
      '2 still open',
    );
    expect(byText('approve'), 'the doomed approve was offered').toBeNull();
    expect(byText('send back'), 'the send-back is the one that works, and it went').not.toBeNull();
  });

  it('offers the approve when the row has nothing left to close', () => {
    summon(
      wireWith([
        row(
          task('t1', 'a row', 'waiting', {
            waiting_on: { kind: 'decision', detail: null, on: null, verification: true },
          }),
        ),
      ]),
    );
    expect(byText('approve'), 'a row that can complete lost its approve').not.toBeNull();
  });

  /**
   * Every card draws whole: its mark, its subject, its meta line and its
   * measure line - owned or not. An unclaimed card says so rather than
   * leaving the meta blank.
   */
  it('draws each card whole, owned or not', () => {
    summon(
      wireWith([
        row(
          task('o1', 'owned', 'in_progress', {
            owner: { org: 'TestOrg', project: 'proj', label: 'lead' },
          }),
        ),
        row(task('u1', 'unclaimed', 'pending')),
      ]),
    );

    const cards = document.querySelectorAll('.b-lanes .b-card');
    expect(cards.length, 'a card per row').toBe(2);
    for (const card of cards) {
      expect(card.querySelector('.b-mark'), 'the mark').not.toBeNull();
      expect(card.querySelector('.b-row1 .b-sub'), 'the subject').not.toBeNull();
      expect(card.querySelector('.b-meta'), 'the meta line').not.toBeNull();
      expect(card.querySelector('.b-last .b-prog'), 'the measure line').not.toBeNull();
    }
    const unowned = [...cards].find((card) => card.querySelector('.b-un') !== null);
    expect(unowned, 'the unclaimed card names its owner-less state').not.toBeUndefined();
  });
});
