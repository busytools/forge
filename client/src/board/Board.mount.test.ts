// @vitest-environment jsdom
import { flushSync, mount, unmount } from 'svelte';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { homeWire } from '../dev/fixture.data';
import { violationsOf } from '../testing/axe';
import type { BoardRow, HomeWire, Marks, Task, TaskStatus } from '../wire/home';
import Board from './Board.svelte';
import Frames from './testing/Frames.svelte';

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
    history: [],
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

    // Onto the Waiting lane: the row moves state and carries its place, and
    // the card lands in that lane at once - before any wire snapshot could
    // arrive.
    drag(() => document.querySelector('.b-lane[data-state="waiting"]'));
    expect(acts[0]).toEqual({
      task_move: { project: 'proj', id: 't2', to: 'waiting', place: { before: null } },
    });
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
    summon(wireWith([row(task('t1', 'a row', 'waiting'))]));
    const press = (key: string): void => {
      // Re-queried per press: the card is drawn in its new lane between them.
      const card = document.querySelector<HTMLElement>('.b-card[data-id="t1"]');
      if (card === null) throw new Error('the card is not on the page');
      card.dispatchEvent(new KeyboardEvent('keydown', { key, bubbles: true }));
      flushSync();
    };

    // A middle lane, so both directions have a neighbour to walk to: the
    // drawn order is Backlog, Waiting, In progress, ... and the card's own
    // state only moves when the wire answers.
    press('ArrowRight');
    expect(acts[0]).toEqual({ task_move: { project: 'proj', id: 't1', to: 'in_progress' } });
    expect(
      document.querySelector('.b-lane[data-state="in_progress"] .b-card[data-id="t1"]'),
      'the card is in the lane the key sent it to',
    ).not.toBeNull();
    press('ArrowLeft');
    expect(acts[1]).toEqual({ task_move: { project: 'proj', id: 't1', to: 'pending' } });
  });

  /**
   * **A keyboard lane step asks for the state alone.** The wire carries the
   * row where the core put it, which for a place-less move is not necessarily
   * last in the lane - a step that recorded itself as ranked would demand the
   * place, call the edit unlanded after the bound, and say it failed while
   * the core had taken it.
   */
  it('lands a keyboard lane step on the state alone', async () => {
    vi.useFakeTimers();
    try {
      // The mount's static type does not carry a component's own exports, so
      // the drive handle is narrowed once, here.
      const frame = mount(Frames, {
        target: document.body,
        props: {
          initial: wireWith([row(task('t1', 'stepped', 'pending'))]),
          onact: (command: Record<string, Record<string, unknown>>) => acts.push(command),
        },
      }) as unknown as { set: (next: HomeWire) => void };
      flushSync();

      const card = document.querySelector<HTMLElement>('.b-card[data-id="t1"]');
      if (card === null) throw new Error('the card is not on the page');
      card.dispatchEvent(new KeyboardEvent('keydown', { key: 'ArrowRight', bubbles: true }));
      flushSync();
      expect(acts[0]).toEqual({ task_move: { project: 'proj', id: 't1', to: 'waiting' } });

      // The server's answer: the row is in waiting, FIRST - not where a place
      // would have put it, because the step asked for none.
      frame.set(
        wireWith([
          row(task('t1', 'stepped', 'waiting', { rank: 1 })),
          row(task('stay', 'already waiting', 'waiting', { rank: 2 })),
        ]),
      );
      flushSync();
      // The clock ticks every 5s and the bound is 12s, so the tick that reads
      // past it is the one at 15s.
      vi.advanceTimersByTime(16_000);
      flushSync();

      expect(document.querySelector('.b-said'), 'the step read as a failed edit').toBeNull();
      expect(
        document.querySelector('.b-lane[data-state="waiting"] .b-card[data-id="t1"]'),
        'the row sits where the wire put it',
      ).not.toBeNull();
      await unmount(frame);
    } finally {
      vi.useRealTimers();
    }
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
        // Two children the close still waits on, and one it does not: a
        // canceled child is terminal, so it must not be what holds the root.
        row(task('c1', 'a child', 'in_progress', { parent: 'epic' })),
        row(task('c2', 'another child', 'pending', { parent: 'epic' })),
        row(task('c3', 'a canceled one', 'canceled', { parent: 'epic' })),
      ]),
    );

    expect(document.body.textContent, 'the strip did not say what holds it').toContain(
      '2 not finished',
    );
    expect(byText('approve'), 'the doomed approve was offered').toBeNull();
    expect(byText('send back'), 'the send-back is the one that works, and it went').not.toBeNull();
  });

  /**
   * And a CHILD in the gate keeps its approve however its own children look:
   * only a root's completion closes a tree, so nothing about a child's
   * children is a refusal the core would make.
   */
  it("leaves a child's approve alone while its own children are open", () => {
    summon(
      wireWith([
        row(task('root', 'the root', 'pending')),
        row(
          task('kid', 'a child', 'waiting', {
            parent: 'root',
            waiting_on: { kind: 'decision', detail: null, on: null, verification: true },
          }),
          { rollup: [0, 1] },
        ),
        row(task('grand', 'a grandchild', 'pending', { parent: 'kid' })),
      ]),
    );
    expect(byText('approve'), 'a child row lost the approve the core accepts').not.toBeNull();
    expect(document.querySelector('.b-hold'), 'a child row drew a hold').toBeNull();
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

  /**
   * **The measure line's words, with their spaces.** The compiler trims a
   * leading space in markup, so the string the card draws is pinned whole -
   * the fix this piece carries - and with it the tree's two marks: a
   * parent's card carries `epic`, a child's `tree`.
   */
  it('draws the measure line whole and the tree marks', () => {
    summon(
      wireWith([
        row(task('epic', 'the epic', 'pending'), { rollup: [1, 2] }),
        row(
          task('child', 'a child', 'in_progress', {
            parent: 'epic',
            estimate: { words: '1d', secs: 86_400 },
          }),
          { worked_secs: 90_000 },
        ),
      ]),
    );

    expect(
      document.querySelector('.b-card[data-id="child"] .b-time')?.textContent,
      'the space before the slash survived',
    ).toBe('worked 25h / 1d');
    expect(
      document.querySelector('.b-card[data-id="epic"]')?.classList.contains('epic'),
      "a parent's card wears the epic spine",
    ).toBe(true);
    expect(
      document.querySelector('.b-card[data-id="child"]')?.classList.contains('tree'),
      "a child's card wears the tree spine",
    ).toBe(true);
  });

  /**
   * **The epic's doors**: a child's pill jumps to its parent, and the
   * parent's own page lists its tasks, each a door onto its card. The
   * epic's own pill is a label, not a control.
   */
  it('jumps between an epic and its tasks', async () => {
    // jsdom performs no layout: scrollIntoView is a browser call the jump
    // makes, so it is stubbed for the length of the case.
    const original = Object.getOwnPropertyDescriptor(Element.prototype, 'scrollIntoView');
    Object.defineProperty(Element.prototype, 'scrollIntoView', {
      value: (): void => {},
      configurable: true,
      writable: true,
    });
    summon(
      wireWith([
        row(task('epic', 'the epic', 'pending'), { rollup: [1, 2] }),
        row(task('child', 'a child', 'waiting', { parent: 'epic' })),
      ]),
    );

    // The child names its epic in a pill and jumps to it.
    const pill = document.querySelector<HTMLElement>('button[aria-label="jump to the epic"]');
    if (pill === null) throw new Error('the epic pill is not on the page');
    expect(pill.textContent).toContain('the epic');
    pill.click();
    flushSync();
    const epicCard = document.querySelector<HTMLElement>('.b-card[data-id="epic"]');
    expect(document.activeElement, 'the jump lands on the epic').toBe(epicCard);
    expect(epicCard?.classList.contains('b-flash'), 'and the card flashes').toBe(true);

    // The epic's pill is a label: a span, not a button.
    const mark = epicCard?.querySelector('.b-epic-pill');
    expect(mark?.tagName, 'the epic pill is a label').toBe('SPAN');

    // Its page lists its tasks; picking one closes the page and lands on
    // the task's card.
    if (epicCard === null) throw new Error('no epic card');
    epicCard.dispatchEvent(
      new MouseEvent('pointerdown', { bubbles: true, clientX: 10, clientY: 10, button: 0 }),
    );
    window.dispatchEvent(new MouseEvent('pointerup', { clientX: 10, clientY: 10 }));
    flushSync();
    expect(document.querySelector('.dr'), 'the press opens the page').not.toBeNull();
    const taskRow = document.querySelector<HTMLElement>('.dr .dr-kid');
    if (taskRow === null) throw new Error('the task is not listed on the page');
    expect(taskRow.textContent).toContain('a child');
    taskRow.click();
    flushSync();
    await vi.waitFor(() => expect(document.querySelector('.dr')).toBeNull());
    expect(document.activeElement, 'the pick lands on the task').toBe(
      document.querySelector('.b-card[data-id="child"]'),
    );

    if (original !== undefined) {
      Object.defineProperty(Element.prototype, 'scrollIntoView', original);
    } else {
      Reflect.deleteProperty(Element.prototype, 'scrollIntoView');
    }
  });
});

describe("the row's own page", () => {
  /** One row with two moves behind it, as the core records them. */
  const withHistory = (entry: BoardRow): BoardRow => ({
    ...entry,
    history: [
      {
        task_id: entry.task.id,
        project_name: 'proj',
        from: null,
        to: 'pending',
        at: { secs_since_epoch: 1_700_000_000, nanos_since_epoch: 0 },
        by: 'user',
      },
      {
        task_id: entry.task.id,
        project_name: 'proj',
        from: 'pending',
        to: 'in_progress',
        at: { secs_since_epoch: 1_700_000_600, nanos_since_epoch: 0 },
        by: { seat: { org: 'TestOrg', project: 'proj', label: 'lead' } },
      },
    ],
  });

  const press = (card: Element, to?: { x: number; y: number }): void => {
    card.dispatchEvent(
      new MouseEvent('pointerdown', { bubbles: true, clientX: 10, clientY: 10, button: 0 }),
    );
    if (to !== undefined) {
      // jsdom performs no layout and carries no elementFromPoint, so the hit
      // test is stubbed: the drag's point resolves to nothing, which lands
      // the card on no lane and no seat.
      const original = Object.getOwnPropertyDescriptor(document, 'elementFromPoint');
      document.elementFromPoint = () => null;
      window.dispatchEvent(new MouseEvent('pointermove', { clientX: to.x, clientY: to.y }));
      if (original !== undefined) {
        Object.defineProperty(document, 'elementFromPoint', original);
      } else {
        Reflect.deleteProperty(document, 'elementFromPoint');
      }
    }
    window.dispatchEvent(new MouseEvent('pointerup', { clientX: 10, clientY: 10 }));
    flushSync();
  };

  /**
   * **A press that never moves is the click**, and a drag is not: the card's
   * page opens with the row's own words and the moves the core recorded.
   */
  it('opens the page on a click, with the row its history, and not on a drag', async () => {
    const held = row(
      task('h1', 'the row to open', 'in_progress', {
        owner: { org: 'TestOrg', project: 'proj', label: 'lead' },
      }),
    );
    summon(wireWith([withHistory(held)]));
    const card = document.querySelector('.b-card[data-id="h1"]');
    if (card === null) throw new Error('no card');

    press(card);
    const drawer = document.querySelector('.dr');
    expect(drawer, 'the click opens the row page').not.toBeNull();
    expect(drawer?.textContent, "the row's own words").toContain('the row to open');
    expect(drawer?.textContent, 'and the moves the core recorded').toContain(
      'moved to in progress by lead',
    );

    // Escape steps back through the drawer's history entry, and a pop is a
    // task: the close lands on it.
    window.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true }));
    flushSync();
    await vi.waitFor(() => expect(document.querySelector('.dr')).toBeNull());

    press(card, { x: 80, y: 80 });
    expect(document.querySelector('.dr'), 'a drag is not a click').toBeNull();
  });

  it('gives the keyboard back to the card when the page closes', async () => {
    summon(wireWith([row(task('k1', 'keyboarded', 'pending'))]));
    const card = document.querySelector<HTMLElement>('.b-card[data-id="k1"]');
    if (card === null) throw new Error('no card');
    card.focus();
    card.dispatchEvent(new KeyboardEvent('keydown', { key: 'Enter', bubbles: true }));
    flushSync();
    expect(document.querySelector('.dr'), 'Enter opens the row page').not.toBeNull();

    window.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true }));
    flushSync();
    await vi.waitFor(() => expect(document.activeElement).toBe(card));
  });

  it('answers a decision from the row page, with the row it answers', () => {
    summon(
      wireWith([
        row(
          task('w1', 'needs you', 'waiting', {
            waiting_on: { kind: 'decision', detail: 'the words', on: null, verification: false },
          }),
        ),
      ]),
    );
    const card = document.querySelector('.b-card[data-id="w1"]');
    if (card === null) throw new Error('no card');
    press(card);

    const answer = document.querySelector<HTMLInputElement>('.dr input[placeholder="your answer"]');
    if (answer === null) throw new Error('the row page carries no answer box');
    answer.value = 'keep the window raise';
    answer.dispatchEvent(new Event('input'));
    flushSync();
    const button = [...document.querySelectorAll<HTMLButtonElement>('.dr .dr-btn')].find(
      (candidate) => candidate.textContent?.trim() === 'answer',
    );
    button?.click();
    flushSync();
    expect(acts.at(-1)).toEqual({
      task_answer: { project: 'proj', id: 'w1', words: 'keep the window raise' },
    });
  });

  /**
   * The done mark is the house's tick, not a coloured blob: the chat draws a
   * settled turn the same way, and one glyph per meaning is the point.
   */
  it("draws the house's tick for a completed row", () => {
    summon(wireWith([row(task('c1', 'shipped', 'completed'))]));
    expect(
      document.querySelector('.b-card .b-mark.b-done')?.innerHTML,
      'the done mark is the check',
    ).toContain('i-check');
    expect(
      document.querySelector('.b-card .b-mark.b-done')?.innerHTML,
      'and not the fail cross',
    ).not.toContain('i-x');
  });

  it("keeps the board reachable through Android's Back", () => {
    summon(wireWith([row(task('b1', 'backed', 'pending'))]));
    const card = document.querySelector('.b-card[data-id="b1"]');
    if (card === null) throw new Error('no card');
    press(card);
    expect(document.querySelector('.dr'), 'the click opens the row page').not.toBeNull();

    // The hardware Back is a pop: the drawer closes and consumes its entry.
    window.dispatchEvent(new PopStateEvent('popstate'));
    flushSync();
    expect(document.querySelector('.dr'), 'the pop closed the row page').toBeNull();
  });

  /**
   * **The gate is the core's rule, not the rollup.** A root whose children are
   * all TERMINAL - one of them failed - may close (the core counts
   * `Completed | Failed | Canceled`), where the rollup still reads not-done;
   * and a child row is never gated at all. The drawer must offer exactly what
   * the strip offers.
   */
  const gated = (open: TaskStatus): BoardRow[] => [
    row(
      task('e1', 'the epic', 'waiting', {
        waiting_on: { kind: 'decision', detail: 'the words', on: null, verification: true },
      }),
    ),
    row(task('c1', 'a child', open, { parent: 'e1' })),
  ];

  const approveButton = (): HTMLButtonElement | undefined =>
    [...document.querySelectorAll<HTMLButtonElement>('.dr .dr-btn')].find(
      (candidate) => candidate.textContent?.trim() === 'approve',
    );

  const openFirst = (): void => {
    const card = document.querySelector('.b-card[data-id="e1"]');
    if (card === null) throw new Error('no epic card');
    press(card);
  };

  it('offers approve when every child is terminal, even a failed one', () => {
    summon(wireWith(gated('failed')));
    openFirst();
    expect(
      approveButton(),
      'a failed child still closes, so the core allows it',
    ).not.toBeUndefined();
  });

  it("holds approve with the strip's words while a child is open", () => {
    summon(wireWith(gated('in_progress')));
    openFirst();
    expect(approveButton(), 'a press the core refuses must not be offered').toBeUndefined();
    expect(
      document.querySelector('.dr-hold')?.textContent,
      'the hold says what is missing',
    ).toContain('not finished');
  });

  it('faces axe with the row page open', async () => {
    const held = row(
      task('a1', 'axe it', 'waiting', {
        waiting_on: { kind: 'decision', detail: 'pick one', on: null, verification: false },
      }),
    );
    summon(wireWith([withHistory(held)]));
    const card = document.querySelector('.b-card[data-id="a1"]');
    if (card === null) throw new Error('no card');
    press(card);
    expect(document.querySelector('.dr'), 'the row page is open').not.toBeNull();
    expect(await violationsOf(document.body.innerHTML)).toEqual([]);
  });

  /**
   * **A parent's page lists its tasks; a childless page draws no list - and
   * both face axe with the marks' controls on screen.** The tree draws on
   * the page a child's card opens too (the parent pill, the head spine, the
   * jump), so the scans run over both shapes.
   */
  it("lists a parent's tasks, none for a childless row, and faces axe alike", async () => {
    summon(
      wireWith([
        row(task('epic', 'the epic', 'pending'), { rollup: [1, 2] }),
        row(
          task('child', 'a child', 'waiting', {
            parent: 'epic',
            estimate: { words: '1d', secs: 86_400 },
          }),
          // The parent's NAME rides the wire's own field, which is what the
          // row's page reads for its spine and pill.
          { worked_secs: 90_000, parent_subject: 'the epic' },
        ),
      ]),
    );
    const open = (id: string): void => {
      const card = document.querySelector<HTMLElement>(`.b-card[data-id="${id}"]`);
      if (card === null) throw new Error(`no card ${id}`);
      press(card);
      flushSync();
    };

    open('child');
    expect(document.querySelector('.dr'), 'the page is open').not.toBeNull();
    expect(document.querySelector('.dr .dr-kid'), 'a childless page drew a task list').toBeNull();
    const worked = [...document.querySelectorAll('.dr dt')].find(
      (t) => t.textContent?.trim() === 'worked',
    );
    expect(worked?.nextElementSibling?.textContent, 'the worked line keeps its space').toBe(
      '25h of 1d',
    );
    expect(document.querySelector('.dr-head.tree'), 'a child page wears the spine').not.toBeNull();
    expect(await violationsOf(document.body.innerHTML)).toEqual([]);
    window.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true }));
    flushSync();
    await vi.waitFor(() => expect(document.querySelector('.dr')).toBeNull());

    open('epic');
    const heading = document.querySelector('.dr h3');
    expect(heading?.textContent, 'the list is headed').toContain('Its tasks');
    expect(
      [...document.querySelectorAll('.dr .dr-kid .l')].some((l) =>
        l.textContent?.includes('a child'),
      ),
      'the child is listed',
    ).toBe(true);
    expect(await violationsOf(document.body.innerHTML)).toEqual([]);
  });

  it('opens on Space as it does on Enter', () => {
    summon(wireWith([row(task('s1', 'spacey', 'pending'))]));
    const card = document.querySelector<HTMLElement>('.b-card[data-id="s1"]');
    if (card === null) throw new Error('no card');
    card.focus();
    card.dispatchEvent(new KeyboardEvent('keydown', { key: ' ', bubbles: true }));
    flushSync();
    expect(document.querySelector('.dr'), 'Space opens the row page').not.toBeNull();
  });
});

describe("the completed group's fold", () => {
  /** A finished row whose last move was `secs` - for a completed row, the moment it finished. */
  const done = (id: string, secs: number, rank: number, over: Partial<Task> = {}): BoardRow =>
    row(
      task(id, `subject ${id}`, 'completed', {
        rank,
        updated_at: { secs_since_epoch: secs, nanos_since_epoch: 0 },
        ...over,
      }),
    );

  const lane = '.b-lane[data-state="completed"]';
  const drawn = (): (string | undefined)[] =>
    [...document.querySelectorAll<HTMLElement>(`${lane} .b-card`)].map((card) => card.dataset.id);

  /**
   * **The finished rows fold.** A board that draws every completed row puts
   * a project's whole history between the reader and the work. The five most
   * recently finished draw by default - chosen by when they last moved, not
   * by their place in the lane, so a row that finished late shows whatever
   * its rank - and the toggle opens the rest. The lane's count stays the
   * true total: the fold hides rows without hiding how many there are.
   */
  it('folds the finished rows to the five most recent, and the toggle opens the rest', async () => {
    summon(
      wireWith([
        done('c1', 100, 1),
        done('c2', 10, 2),
        done('c3', 20, 3),
        done('c4', 30, 4),
        done('c5', 40, 5),
        done('c6', 90, 6),
        done('c7', 80, 7),
      ]),
    );

    // The five latest finishes (100, 90, 80, 40, 30), drawn where the lane's
    // own order puts them; c2 and c3 finished before those and fold away.
    expect(drawn(), 'the older rows were not held back').toEqual(['c1', 'c4', 'c5', 'c6', 'c7']);
    expect(
      document.querySelector(`${lane} .b-count`)?.textContent?.trim(),
      'the count stopped naming the whole group',
    ).toBe('7');

    const toggle = document.querySelector<HTMLButtonElement>(`${lane} .b-fold`);
    if (toggle === null) throw new Error('the finished group drew no fold control');
    expect(toggle.textContent?.trim(), 'the toggle names the whole group').toBe('show all (7)');
    expect(toggle.getAttribute('aria-expanded'), 'the toggle reads as closed').toBe('false');

    toggle.click();
    flushSync();
    expect(drawn(), 'the toggle did not open the rest').toEqual([
      'c1',
      'c2',
      'c3',
      'c4',
      'c5',
      'c6',
      'c7',
    ]);
    expect(toggle.textContent?.trim(), 'the open fold offers the close').toBe('show less');
    expect(toggle.getAttribute('aria-expanded'), 'the toggle reads as open').toBe('true');

    toggle.click();
    flushSync();
    expect(drawn(), 'the toggle did not fold back').toEqual(['c1', 'c4', 'c5', 'c6', 'c7']);

    expect(await violationsOf(document.body.innerHTML)).toEqual([]);
  });

  /** A group small enough to see whole draws whole: nothing folds, and no control offers to open nothing. */
  it('draws a finished group of five or fewer whole, with no fold control', () => {
    summon(
      wireWith([
        done('c1', 100, 1),
        done('c2', 90, 2),
        done('c3', 80, 3),
        done('c4', 70, 4),
        done('c5', 60, 5),
      ]),
    );
    expect(document.querySelectorAll(`${lane} .b-card`)).toHaveLength(5);
    expect(document.querySelector(`${lane} .b-fold`), 'a whole group offered a fold').toBeNull();
  });

  /** And a lane with nothing finished draws its empty self: no rows, count zero, no control. */
  it('draws an empty finished lane with no fold control', () => {
    summon(wireWith([row(task('p1', 'live', 'pending'))]));
    const empty = document.querySelector(lane);
    expect(empty, 'the finished lane went missing').not.toBeNull();
    expect(empty?.querySelector('.b-count')?.textContent?.trim()).toBe('0');
    expect(empty?.querySelector('.b-fold'), 'an empty lane offered a fold').toBeNull();
  });

  /**
   * **A card being dropped into the folded lane stays drawn.** A drop is
   * immediate on the page while the core's answer comes later, and a row
   * that has not finished yet is not one of the five most recent - so a
   * plain fold would swallow the card mid-gesture and give it back only
   * when the wire caught up.
   */
  it('keeps a card the reader drops into the folded lane drawn', () => {
    summon(
      wireWith([
        ...Array.from({ length: 6 }, (_, i) => done(`c${i + 1}`, 100 + i, i + 1)),
        row(task('t1', 'moving', 'pending')),
      ]),
    );

    const original = Object.getOwnPropertyDescriptor(document, 'elementFromPoint');
    document.elementFromPoint = () => document.querySelector(lane);
    const card = document.querySelector<HTMLElement>('.b-card[data-id="t1"]');
    if (card === null) throw new Error('the card is not on the page');
    card.dispatchEvent(
      new MouseEvent('pointerdown', { bubbles: true, clientX: 10, clientY: 10, button: 0 }),
    );
    window.dispatchEvent(new MouseEvent('pointermove', { clientX: 60, clientY: 60 }));
    window.dispatchEvent(new MouseEvent('pointerup', {}));
    flushSync();

    expect(acts[0]).toEqual({
      task_move: { project: 'proj', id: 't1', to: 'completed', place: { before: null } },
    });
    expect(
      document.querySelector(`${lane} .b-card[data-id="t1"]`),
      'the dropped card vanished into the fold',
    ).not.toBeNull();
    expect(
      document.querySelector(`${lane} .b-card[data-id="c1"]`),
      'the fold stopped holding the older rows back',
    ).toBeNull();

    if (original !== undefined) {
      Object.defineProperty(document, 'elementFromPoint', original);
    } else {
      Reflect.deleteProperty(document, 'elementFromPoint');
    }
  });

  /**
   * **Every finished row stays reachable.** A folded row is off the page,
   * and the epic pill and the row page's task list land on cards by id: a
   * jump aimed at a folded card opens the fold and lands, rather than dying
   * on a missing element.
   */
  it('opens the fold to land a jump on a folded row', async () => {
    // jsdom performs no layout: scrollIntoView is a browser call the jump
    // makes, so it is stubbed for the length of the case.
    const original = Object.getOwnPropertyDescriptor(Element.prototype, 'scrollIntoView');
    Object.defineProperty(Element.prototype, 'scrollIntoView', {
      value: (): void => {},
      configurable: true,
      writable: true,
    });
    summon(
      wireWith([
        done('epic', 10, 1, { subject: 'the epic' }),
        done('c2', 120, 2),
        done('c3', 130, 3),
        done('c4', 140, 4),
        done('c5', 150, 5),
        done('c6', 160, 6),
        row(task('kid', 'a child', 'failed', { parent: 'epic' })),
      ]),
    );

    expect(document.querySelector('.b-card[data-id="epic"]'), 'the epic drew unfolded').toBeNull();
    const pill = document.querySelector<HTMLElement>('button[aria-label="jump to the epic"]');
    if (pill === null) throw new Error('the epic pill is not on the page');
    pill.click();
    flushSync();
    await vi.waitFor(() =>
      expect(document.activeElement, 'the jump never landed').toBe(
        document.querySelector('.b-card[data-id="epic"]'),
      ),
    );

    expect(
      document.querySelector('.b-card[data-id="epic"]')?.classList.contains('b-flash'),
      'the landed card flashes',
    ).toBe(true);

    if (original !== undefined) {
      Object.defineProperty(Element.prototype, 'scrollIntoView', original);
    } else {
      Reflect.deleteProperty(Element.prototype, 'scrollIntoView');
    }
  });
});
