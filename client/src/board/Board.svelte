<script lang="ts">
  import { hrefForSlot } from '../routes';
  import { isBoardRefusal, type ServiceReport } from '../wire/fleet';
  import type { HomeWire, TaskStatus } from '../wire/home';
  import { TASK_STATUSES } from '../wire/home';
  import Picker from './Picker.svelte';
  import { applyMoves, boardView, landed } from './view';
  import type { BoardCardView, PendingMove } from './view';

  /**
   * One project's board, opened as a takeover: a slim top bar with back and
   * done, the rows waiting on the reader, cards in lanes by state, and the
   * controls that move them. Nothing of any other project appears here -
   * the fleet is the glance, this is the world.
   *
   * The page is a view: every edit it offers is handed out through `onact`,
   * which the app wires to the socket's command path. Without one the
   * controls still draw - the page's shape does not depend on a live
   * connection - and a press does nothing.
   */
  let {
    wire,
    org,
    project,
    onact = null,
    notice = null,
  }: {
    wire: HomeWire;
    org: string;
    project: string;
    onact?: ((command: Record<string, Record<string, unknown>>) => void) | null;
    /** The core's last service report, which carries a refused edit's words. */
    notice?: ServiceReport | null;
  } = $props();

  const view = $derived(boardView(wire, org, project));
  /**
   * The reader's own drags, applied at once and reconciled against the
   * wire: a card that waits seconds for the server's snapshot reads as a
   * drag that failed.
   */
  let moved = $state<Record<string, PendingMove>>({});
  const lanes = $derived(applyMoves(view.lanes, moved));
  /** The project's seats, for the owner picker: the label that may hold a row. */
  const seats = $derived(
    wire.agents
      .filter((agent) => agent.slot.org === org && agent.slot.project === project)
      .map((agent) => ({ label: agent.label, href: hrefForSlot(agent.slot) })),
  );
  /** The epics a new row can nest under. */
  const epics = $derived(
    wire.projects
      .find((entry) => entry.project.org === org && entry.project.name === project)
      ?.rows.filter((entry) => entry.task.parent === null) ?? [],
  );
  /** The worker's own words on a row it sent back, before it is dispatched. */
  const sendBackWords: Record<string, string> = $state({});
  const answers: Record<string, string> = $state({});
  let subject = $state('');
  let parent = $state('');

  /**
   * Hand one edit to the app, and say so when the socket will not carry it.
   *
   * A dispatch on a closed socket THROWS, and a throw out of a press handler
   * is a press that leaves the page mid-gesture: the drag's own state stays
   * set, the ghost stays up and the card is drawn where the core never put it.
   * So the loss is caught here, said on the page's line, and reported to the
   * caller - the drag only places its card optimistically when the edit went.
   */
  let lost: ServiceReport | null = $state(null);

  function act(command: Record<string, Record<string, unknown>>): boolean {
    try {
      onact?.(command);
      lost = null;
      return true;
    } catch {
      lost = { severity: 'warning', message: 'the socket is closed, so that edit was not sent' };
      return false;
    }
  }

  /**
   * What the page says above the lanes: the local loss, or the core's own
   * words for a board edit it refused. **Only that edit's** - the service
   * line carries every producer's report, and one about a worker's spawn is
   * not news about this board.
   */
  const line = $derived(lost ?? (notice !== null && isBoardRefusal(notice) ? notice : null));

  /** Close the takeover: back the way the reader came, or home from a deep link. */
  function close(): void {
    if (history.length > 1) history.back();
    else location.assign('/');
  }

  /**
   * The keyboard's half of the drag, on a focused card: the arrows do what
   * the pointer does. Up and down re-order the card within its lane; left and
   * right move it to the neighbouring lane, in the order the lanes are drawn,
   * so what the keys walk is what the reader sees. A key that arrives from a
   * control inside the card - the owner picker's own arrows - belongs to that
   * control, not to the card.
   */
  function keyMove(event: KeyboardEvent, row: BoardCardView): void {
    const target = event.target;
    if (target instanceof Element && target.closest('button, input, select, a') !== null) return;
    if (event.key === 'ArrowUp' || event.key === 'ArrowDown') {
      event.preventDefault();
      act({ task_rank: { project, id: row.id, to: event.key === 'ArrowUp' ? 'up' : 'down' } });
      return;
    }
    const step = event.key === 'ArrowLeft' ? -1 : event.key === 'ArrowRight' ? 1 : 0;
    if (step === 0) return;
    const to = laneStep(row.status, step);
    if (to === null) return;
    event.preventDefault();
    const box = boardEl?.querySelector(`.b-card[data-id="${row.id}"]`)?.getBoundingClientRect();
    if (act({ task_move: { project, id: row.id, to } })) {
      moved[row.id] = { to, before: null, ranked: false, at: Date.now() };
      if (box !== undefined) settle(row.id, box);
    }
  }

  /** The lane one step along the drawn order, or nothing at its ends. */
  function laneStep(status: TaskStatus, step: number): TaskStatus | null {
    const keys = view.lanes.map((lane) => lane.key);
    const at = keys.indexOf(status);
    if (at === -1) return null;
    return keys[at + step] ?? null;
  }

  /** The status mark's class, one per state - shape and colour both. */
  function markClass(status: string): string {
    switch (status) {
      case 'in_progress':
        return 'b-run';
      case 'waiting':
        return 'b-wait';
      case 'completed':
        return 'b-done';
      case 'failed':
        return 'b-fail';
      case 'canceled':
        return 'b-cancel';
      default:
        return 'b-pend';
    }
  }

  /** The progress rule under a card's time, as a percentage of the estimate. */
  function fill(row: { ratio: number }): string {
    return `${Math.round(row.ratio * 100)}%`;
  }

  /**
   * A live clock, so "wrote 12s ago" ticks between the server's pushes -
   * a board that only changes when someone else moves reads as dead.
   */
  let nowSecs = $state(Math.floor(Date.now() / 1_000));
  $effect(() => {
    const id = setInterval(() => (nowSecs = Math.floor(Date.now() / 1_000)), 5_000);
    return () => clearInterval(id);
  });

  /** How long since the row last moved, in the board's words. */
  function ago(secs: number): string {
    const d = Math.max(0, nowSecs - secs);
    if (d >= 3_600) return `${Math.floor(d / 3_600)}h`;
    if (d >= 60) return `${Math.floor(d / 60)}m`;
    return `${d}s`;
  }

  /** A row is live while it is running and wrote within the last two minutes. */
  function live(row: { status: string; updatedAt: number }): boolean {
    return row.status === 'in_progress' && nowSecs - row.updatedAt < 120;
  }

  /** The seat's page, when the owner is a seat this project holds. */
  function ownerHref(label: string): string | null {
    return seats.find((seat) => seat.label === label)?.href ?? null;
  }

  /**
   * Dragging: a card follows the pointer, and what it lands ON decides the
   * command - another lane is a move, a position in its own lane is a rank,
   * a seat is an assignment. A press that never passes the threshold is a
   * click, and the arrows on a focused card are the keyboard's half.
   */
  let pending: { id: string; x0: number; y0: number } | null = null;
  let dragging: { id: string; subject: string; x: number; y: number } | null = $state(null);
  let dropLane: TaskStatus | null = $state(null);
  let dropSeat: string | null = $state(null);
  let dropBefore: string | null = $state(null);

  /** The card a row id belongs to, across the lanes. */
  function cardOf(id: string): BoardCardView | undefined {
    return view.lanes.flatMap((lane) => lane.cards).find((row) => row.id === id);
  }

  /**
   * A lane's `data-state` as a status. The attribute is only ever written
   * from a lane's own key, so the value is one of the statuses; anything
   * else (a stray element carrying the attribute) resolves to nothing and
   * the drop goes unclaimed rather than moving a row somewhere invented.
   */
  function laneState(value: string | null): TaskStatus | null {
    return value !== null && (TASK_STATUSES as readonly string[]).includes(value)
      ? (value as TaskStatus)
      : null;
  }

  /**
   * How long a drag's own placement waits for the wire to agree with it.
   *
   * A refused edit, or one the core found nothing to apply, sends no frame
   * this page can match - so without a bound the card would sit in a lane the
   * core never accepted for the rest of the session. The board's own tick is
   * the clock, so the bound costs no second timer.
   */
  const SETTLE_SECS = 12;

  /**
   * Drop a drag's overlay once the wire shows the row where it was put - or
   * once the core has had long enough to say otherwise, in which case the card
   * goes back to the wire's own answer and the page says so.
   */
  $effect(() => {
    const snapshot = wire;
    const tick = nowSecs;
    const row = snapshot.projects.find(
      (entry) => entry.project.org === org && entry.project.name === project,
    );
    for (const [id, want] of Object.entries(moved)) {
      const here = landed(row?.rows ?? [], id, want);
      const waited = tick - Math.floor(want.at / 1_000) > SETTLE_SECS;
      if (here || waited) {
        delete moved[id];
        if (!here) {
          lost = {
            severity: 'warning',
            message: "that edit did not land, so the card went back to the core's own answer",
          };
        }
      }
    }
  });

  function down(event: PointerEvent, row: BoardCardView): void {
    if (event.button !== 0) return;
    // A press on a control inside the card is that control's, not a drag's.
    const target = event.target;
    if (target instanceof Element && target.closest('button, input, select, a') !== null) return;
    // Without this the press starts a text selection, and every pointermove
    // drags the highlight across the board.
    event.preventDefault();
    // And it takes the focus a press would have given the card with it, so
    // the arrows would only ever reach a card that Tab reached.
    if (event.currentTarget instanceof HTMLElement) event.currentTarget.focus();
    pending = { id: row.id, x0: event.clientX, y0: event.clientY };
  }

  function move(event: PointerEvent): void {
    if (pending === null) return;
    if (dragging === null) {
      if (Math.abs(event.clientX - pending.x0) + Math.abs(event.clientY - pending.y0) < 6) return;
      const card = cardOf(pending.id);
      dragging = {
        id: pending.id,
        subject: card?.subject ?? '',
        x: event.clientX,
        y: event.clientY,
      };
    } else {
      dragging.x = event.clientX;
      dragging.y = event.clientY;
    }
    // What is under the pointer: a seat first, then a lane.
    const under = document.elementFromPoint(event.clientX, event.clientY);
    const seat = under instanceof Element ? under.closest('[data-seat]') : null;
    const lane = under instanceof Element ? under.closest('[data-state]') : null;
    dropSeat = seat?.getAttribute('data-seat') ?? null;
    dropLane = dropSeat === null ? laneState(lane?.getAttribute('data-state') ?? null) : null;
    dropBefore =
      dropLane === null || lane === null ? null : beforeAt(lane, event.clientY, dragging.id);
  }

  /** The id of the card a drop at `y` should sit above, or null for the end. */
  function beforeAt(lane: Element, y: number, draggedId: string): string | null {
    const cards = [...lane.querySelectorAll('.b-card')].filter(
      (card) => card.getAttribute('data-id') !== draggedId,
    );
    for (const card of cards) {
      const rect = card.getBoundingClientRect();
      if (y < rect.top + rect.height / 2) return card.getAttribute('data-id');
    }
    return null;
  }

  /** The board element, for the drop's own animation. */
  let boardEl: HTMLElement | undefined = $state();

  /**
   * A dropped card flies from where it was to where it landed: the element
   * moves between lanes in one repaint, so the motion comes from measuring
   * the two positions and playing the difference.
   */
  function settle(id: string, from: DOMRect): void {
    // jsdom carries no matchMedia; a browser that has none animates.
    const wants =
      typeof window.matchMedia === 'function' &&
      window.matchMedia('(prefers-reduced-motion: reduce)').matches;
    if (wants) return;
    requestAnimationFrame(() => {
      const el = boardEl?.querySelector(`.b-card[data-id="${id}"]`);
      if (!(el instanceof HTMLElement)) return;
      const to = el.getBoundingClientRect();
      const dx = from.left - to.left;
      const dy = from.top - to.top;
      if (dx === 0 && dy === 0) return;
      el.animate([{ transform: `translate(${dx}px, ${dy}px)` }, { transform: 'none' }], {
        duration: 200,
        easing: 'cubic-bezier(.22,.61,.36,1)',
      });
    });
  }

  function up(): void {
    if (dragging !== null) {
      const card = cardOf(dragging.id);
      const box = boardEl
        ?.querySelector(`.b-card[data-id="${dragging.id}"]`)
        ?.getBoundingClientRect();
      if (dropSeat !== null) {
        act({ task_assign: { project, id: dragging.id, owner: dropSeat } });
      } else if (dropLane !== null && card !== undefined && card.status !== dropLane) {
        // The card lands where it was dropped right away; the wire catches
        // up. Only when the edit went: a move the socket never took must not
        // leave the card where the core will never put it. `ranked: false` -
        // a drop onto another lane asks for the state, not the position.
        if (act({ task_move: { project, id: dragging.id, to: dropLane } })) {
          moved[dragging.id] = {
            to: dropLane,
            before: dropBefore,
            ranked: false,
            at: Date.now(),
          };
          if (box !== undefined) settle(dragging.id, box);
        }
      } else if (dropLane !== null) {
        if (act({ task_rank: { project, id: dragging.id, to: { before: dropBefore } } })) {
          moved[dragging.id] = { to: dropLane, before: dropBefore, ranked: true, at: Date.now() };
          if (box !== undefined) settle(dragging.id, box);
        }
      }
    }
    pending = null;
    dragging = null;
    dropLane = null;
    dropSeat = null;
    dropBefore = null;
  }
</script>

<svelte:window onpointermove={move} onpointerup={up} onpointercancel={up} />

<main class="wrap board" class:dragging={dragging !== null} bind:this={boardEl}>
  <div class="b-top" class:dragging={dragging !== null}>
    <button type="button" class="b-back" onclick={close}>{'\u{2190}'} back</button>
    <span class="b-id">
      <span class="b-nm">{project}</span>
      <span class="b-read">
        {#if view.read.onYou > 0}<b>{view.read.onYou} waiting on you</b> &middot;
        {/if}{view.read.rows} rows &middot; {view.read.running} running
      </span>
    </span>
    <span class="b-spacer"></span>
    {#each seats as seat (seat.label)}
      <a class="b-seat" class:drop={dropSeat === seat.label} data-seat={seat.label} href={seat.href}
        >{seat.label}</a
      >
    {/each}
    <button type="button" class="b-done" onclick={close}>done</button>
  </div>

  {#if line !== null}
    <!-- The core's own words for an edit that did not land. A press that did
         nothing and a press the core refused must not read the same. -->
    <p class="b-said {line.severity}" role="status">{line.message}</p>
  {/if}

  {#if view.waiting.length > 0}
    <section class="b-you" aria-label="waiting on you">
      <h2>waiting on you <span class="b-n">{view.waiting.length}</span></h2>
      <div class="b-asks">
        {#each view.waiting as row (row.id)}
          <div class="b-ask">
            <div class="b-q">{row.subject}</div>
            <div class="b-why">
              {#if row.owner !== null}<span>{row.owner}</span>{/if}
              <span>{row.age}</span>
            </div>
            {#if row.verification}
              {#if row.detail !== null}<div class="b-detail">{row.detail}</div>{/if}
              <div class="b-do">
                {#if row.root && row.open > 0}
                  <!-- A root cannot complete while a child is non-terminal,
                       so an approve here is a press the core refuses. The
                       strip says what it still waits on, and offers only the
                       send-back - which the core does take. A CHILD's approve
                       closes no tree, so nothing is held there. -->
                  <span class="b-hold"
                    >{row.rollup} done &middot; {row.open} not finished, and a root closes when its children
                    are</span
                  >
                {:else}
                  <button
                    type="button"
                    class="b-btn b-ok"
                    onclick={() => act({ task_verdict: { project, id: row.id, approve: true } })}
                    >approve</button
                  >
                {/if}
                <input
                  class="b-in"
                  data-editor="board"
                  type="text"
                  placeholder="what to change (sent back with it)"
                  bind:value={sendBackWords[row.id]}
                />
                <button
                  type="button"
                  class="b-btn b-no"
                  onclick={() =>
                    act({
                      task_verdict: {
                        project,
                        id: row.id,
                        approve: false,
                        words: sendBackWords[row.id] ?? '',
                      },
                    })}>send back</button
                >
              </div>
            {:else}
              {#if row.detail !== null}<div class="b-detail">{row.detail}</div>{/if}
              <div class="b-do">
                <input
                  class="b-in"
                  data-editor="board"
                  type="text"
                  placeholder="your answer"
                  bind:value={answers[row.id]}
                />
                <button
                  type="button"
                  class="b-btn b-plain"
                  onclick={() =>
                    act({ task_answer: { project, id: row.id, words: answers[row.id] ?? '' } })}
                  >answer</button
                >
              </div>
            {/if}
          </div>
        {/each}
      </div>
    </section>
  {/if}

  {#if !view.known}
    <!-- The page is a route and this one names a project the fleet does not
         carry: it has no rows to draw and no board to edit, and the create
         line would file a row against nothing. -->
    <p class="b-empty">
      {project} is not one of this forge's projects - check the name in forge.toml, or open the board
      from its row on the home.
    </p>
  {:else}
    {#if view.empty}
      <p class="b-empty">
        nothing owned, nothing waiting - cut a row below, or tell the lead what is next
      </p>
    {/if}

    <div class="b-lanes">
      {#each lanes as lane (lane.key)}
        <section
          class="b-lane b-lane-{lane.key}"
          class:drop={dropLane === lane.key}
          data-state={lane.key}
        >
          <div class="b-cap">
            <span class="b-lane-nm">{lane.name}</span>
            <span class="b-count">{lane.count}</span>
            <span class="b-rule"></span>
          </div>
          {#each lane.cards as row (row.id)}
            <!-- The card itself is the interaction: it takes the pointer for a
               drag and the arrows for the keyboard's half of it - up and down
               re-order the row, left and right move it between lanes - so it
               is focusable without being a widget role. `option` and `button`
               are lies over a card that holds a link and a picker, and hidden
               buttons per card would be more tab stops for the moves the
               arrows already reach. -->
            <!-- svelte-ignore a11y_no_noninteractive_tabindex -->
            <!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
            <article
              class="b-card"
              class:dragging={dragging?.id === row.id}
              data-id={row.id}
              tabindex="0"
              onpointerdown={(event) => down(event, row)}
              onkeydown={(event) => keyMove(event, row)}
            >
              <div class="b-row1">
                <span class="b-mark {markClass(row.status)}" aria-hidden="true"></span>
                <span class="b-sub" class:settled={row.status === 'completed'} title={row.subject}>
                  <span class="b-line">{row.display}</span>
                  {#if row.display !== row.subject}<span class="b-sub2">{row.subject}</span>{/if}
                </span>
              </div>
              <div class="b-meta">
                {#if row.owner !== null}
                  {#if ownerHref(row.owner) !== null}
                    <a class="b-owner" href={ownerHref(row.owner)}>
                      <span class="b-ava">{row.initials}</span>{row.owner}
                    </a>
                  {:else}
                    <span class="b-owner"><span class="b-ava">{row.initials}</span>{row.owner}</span
                    >
                  {/if}
                {:else}
                  <span class="b-un">unclaimed</span>
                {/if}
                {#if row.epic !== null}<span class="b-epic">{row.epic}</span>{/if}
                {#each row.links as link (link.label)}
                  {#if link.href !== null}
                    <a href={link.href} target="_blank" rel="noreferrer">{link.label}</a>
                  {:else}
                    <span class="b-link">{link.label}</span>
                  {/if}
                {/each}
                <span class="b-chips">
                  {#if row.rollup !== null}<span class="b-chip">{row.rollup} done</span>{/if}
                  {#each row.chips as chip (chip.label)}
                    <span class="b-chip {chip.tone}">{chip.label}</span>
                  {/each}
                </span>
              </div>
              <div class="b-last">
                <span
                  class="b-prog"
                  class:over={row.tone === 'over'}
                  class:bad={row.tone === 'bad'}
                >
                  <span class="b-time"
                    >worked {row.worked}{#if row.estimate !== null}
                      / {row.estimate}{/if}</span
                  >
                  {#if row.estimate !== null}
                    <span class="b-bar"><span class="b-fill" style="width:{fill(row)}"></span></span
                    >
                  {/if}
                </span>
                <span class="b-wrote" class:live={live(row)}>wrote {ago(row.updatedAt)} ago</span>
                <span class="b-acts">
                  <Picker
                    label="the seat holding {row.subject}"
                    value={row.owner ?? ''}
                    options={[
                      { value: '', label: 'unclaimed' },
                      ...seats.map((seat) => ({ value: seat.label, label: seat.label })),
                    ]}
                    onpick={(value) =>
                      act({
                        task_assign: {
                          project,
                          id: row.id,
                          owner: value === '' ? null : value,
                        },
                      })}
                  />
                </span>
              </div>
            </article>
          {/each}
        </section>
      {/each}
    </div>

    <div class="b-create">
      <input
        class="b-in"
        data-editor="board"
        type="text"
        placeholder="a new row"
        bind:value={subject}
      />
      <Picker
        label="the epic it belongs under"
        value={parent}
        options={[
          { value: '', label: 'no epic' },
          ...epics.map((epic) => ({ value: epic.task.id, label: epic.task.subject })),
        ]}
        onpick={(value: string) => {
          parent = value;
        }}
      />
      <button
        type="button"
        class="b-btn b-add"
        onclick={() => {
          act({
            task_create: {
              project,
              subject,
              parent: parent === '' ? null : parent,
            },
          });
          subject = '';
        }}>+ add</button
      >
    </div>
  {/if}

  {#if dragging !== null}
    <div class="b-ghost" style="left:{dragging.x + 14}px; top:{dragging.y + 10}px">
      {dragging.subject}
    </div>
  {/if}
</main>
