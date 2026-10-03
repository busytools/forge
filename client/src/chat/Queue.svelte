<script lang="ts">
  import Icon from '../components/Icon.svelte';
  import type { Connection } from '../socket';
  import type { QueuedPromptRow } from '../session/wire';
  import type { SessionSlot } from '../wire/types';
  import { faceAt, walk } from './queue';

  /**
   * The prompts waiting in the CLI's queue, drawn as a stack above the box.
   *
   * **The pile is the core's, not this view's.** The seat read carries it and
   * `prompt_queued` / `prompt_lifecycle` keep it current, so two clients agree
   * about what is waiting; this draws it and dispatches a cancel. A row leaves
   * when the core says it left - `started` and the terminal states - so a
   * prompt the CLI already took cannot be drawn as dropped.
   *
   * **One mechanism makes the depth.** Every row is absolutely positioned at
   * the pile's foot and lifted by six pixels per step, so the rows behind the
   * face show as edges rather than as a list: the height the pile occupies is
   * the face plus those steps, and the chat column above shrinks by exactly
   * that much. The face is whichever row the walk is on - the newest when the
   * walk is in the box - and only the face draws its words.
   */
  let {
    rows,
    slot,
    connection,
  }: { rows: QueuedPromptRow[]; slot: SessionSlot; connection: Connection } = $props();

  /** The step between one arc and the next. */
  const STEP = 6;

  /**
   * One row's drawn height, which the container's own height is built from.
   *
   * A constant rather than a measurement, because the app ships ONE type
   * scale (`web.css`): the row is 8+7 of padding, a 1.5-line of prose, 5 of
   * gap, a 1.5-line of mono meta and a hairline, and every row is the same by
   * construction - the words never wrap. Measuring would tie the pile to a
   * `ResizeObserver` for a number the theme already fixes.
   */
  const FACE_HEIGHT = 64;

  /** The row being walked, by uuid: null is the box. */
  let cursor = $state<string | null>(null);

  let pile = $state<HTMLElement | null>(null);

  /**
   * A cursor whose row has left is no cursor: the core settling the pointed
   * row - delivered or dropped - clears it, so the walk starts again at the
   * newest rather than going inert on a uuid nothing draws.
   */
  $effect(() => {
    if (cursor !== null && !rows.some((row) => row.uuid === cursor)) {
      cursor = null;
    }
  });

  /** Where the face sits: the cursor's row, or the newest when the walk is in the box. */
  const face = $derived(faceAt(cursor, rows));

  /**
   * The walk, as the terminal taught it: up enters at the newest and goes
   * older until the first queued prompt, down comes back, and one more down
   * past the newest lands in the box - focus included, so the next Enter is
   * the next send.
   */
  function move(direction: 'up' | 'down'): void {
    const was = cursor;
    cursor = walk(cursor, rows, direction);
    if (cursor === null && was !== null && direction === 'down') {
      draft()?.focus();
    }
  }

  function cancel(uuid: string): void {
    // Fire-and-forget like the composer's send: the answer rides the stream,
    // and the row leaves when the core says it left - `cancelled: false` means
    // the CLI had already taken it, and its own `started` frame settles it.
    void connection.dispatch({ cancel_queued_prompt: { key: slot, uuid } });
  }

  /** The box this pile shares its composer container with. */
  function draft(): HTMLTextAreaElement | null {
    return holder?.querySelector<HTMLTextAreaElement>('textarea') ?? null;
  }

  /** The composer container, kept so the box is still reachable after the pile itself unmounts. */
  let holder: HTMLElement | null = null;
  let focused = $state(false);

  /**
   * Entering the pile - the empty box's up, or a click - lands on the newest,
   * which is where the walk's next up goes older from.
   */
  function onfocus(): void {
    holder = pile?.closest('.composer') ?? null;
    focused = true;
    if (cursor === null && rows.length > 0) {
      cursor = rows[rows.length - 1]?.uuid ?? null;
    }
  }

  /**
   * The pile unmounting while it holds the keyboard hands it back to the box:
   * the last row leaving would otherwise drop focus on the body, and the next
   * keystroke with it.
   */
  $effect(() => {
    if (rows.length === 0 && focused) {
      focused = false;
      draft()?.focus();
    }
  });

  function onkey(event: KeyboardEvent): void {
    if (event.key === 'ArrowUp') {
      event.preventDefault();
      move('up');
      return;
    }
    if (event.key === 'ArrowDown') {
      event.preventDefault();
      move('down');
      return;
    }
    if ((event.key === 'Backspace' || event.key === 'Delete') && cursor !== null) {
      event.preventDefault();
      cancel(cursor);
      return;
    }
    if (event.key === 'Escape') {
      cursor = null;
    }
  }
</script>

{#if rows.length > 0}
  <div class="pile">
    <!-- The head is OUTSIDE the listbox: a listbox holds options and nothing
         else, so the cancel control - the mouse's way to the same act as
         backspace - sits beside it rather than inside an option. -->
    <div class="head">
      <span>queued <b>{rows.length}</b></span>
      {#if cursor !== null}
        {@const held = rows.find((row) => row.uuid === cursor)}
        <button class="del" type="button" onclick={() => cancel(cursor ?? '')}>
          <Icon name="x" class="ic" /> cancel {held?.source ?? ''}
        </button>
      {/if}
      <span class="hint"><b>↑↓</b> walk &middot; <b>⌫</b> cancel</span>
    </div>
    <div
      class="list"
      bind:this={pile}
      tabindex="-1"
      role="listbox"
      aria-label="Queued prompts"
      aria-activedescendant={cursor ?? ''}
      {onfocus}
      onblur={() => (focused = false)}
      onkeydown={onkey}
      onclick={() => pile?.focus()}
    >
      <div class="rows" style="height: {FACE_HEIGHT + face * STEP}px">
        {#each rows as row, at (row.uuid)}
          {@const depth = face - at}
          {#if depth >= 0}
            <div
              class="row"
              class:back={depth > 0}
              class:cur={cursor === row.uuid}
              role="option"
              id={row.uuid}
              tabindex="-1"
              aria-selected={cursor === row.uuid}
              style="transform: translateY({-depth * STEP}px)"
            >
              <span class="w">{row.text}</span>
              <span class="m">
                <span class="src {row.source}">{row.source}</span>
                {#if at === 0}<span class="nx">next</span>{/if}
                {#if cursor === row.uuid}<span class="pos">#{at + 1} / {rows.length}</span>{/if}
              </span>
            </div>
          {/if}
        {/each}
      </div>
    </div>
  </div>
{/if}

<style>
  /* The pile owns its height: the face plus one step per row above it, so the
     column above shrinks by exactly that much and nothing paints over it. */
  .list {
    outline: none;
  }
  .head {
    display: flex;
    align-items: baseline;
    gap: 10px;
    font-family: var(--mono);
    font-size: var(--fs-label);
    color: var(--dim);
    margin-bottom: 5px;
  }
  .head b {
    color: var(--muted);
    font-weight: 400;
  }
  .head .del {
    display: inline-flex;
    align-items: center;
    gap: 4px;
    font-family: var(--mono);
    font-size: var(--fs-label);
    color: var(--dim);
    background: none;
    border: 1px solid var(--line);
    border-radius: 6px;
    padding: 0 7px;
    cursor: pointer;
  }
  .head .del:hover,
  .head .del:focus-visible {
    color: var(--bad);
    border-color: var(--bad);
  }
  .head .hint {
    margin-left: auto;
  }
  .rows {
    position: relative;
  }
  .row {
    position: absolute;
    left: 0;
    right: 0;
    bottom: 0;
    border: 1px solid var(--line);
    border-radius: var(--r);
    background: var(--s2);
    padding: 8px 13px 7px;
  }
  /* An arc is a surface: every row behind the face keeps its words to itself,
     so nothing bleeds through the step. */
  .row.back .w,
  .row.back .m {
    visibility: hidden;
  }
  .row.cur {
    border-color: var(--ctl);
    background: var(--s3);
    box-shadow: 0 12px 28px rgba(0, 0, 0, 0.42);
  }
  .w {
    display: block;
    font-size: var(--fs-prose);
    color: var(--muted);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .row.cur .w {
    color: var(--text);
  }
  .m {
    display: flex;
    align-items: baseline;
    gap: 8px;
    margin-top: 5px;
    font-family: var(--mono);
    font-size: var(--fs-label);
    color: var(--dim);
  }
  .m .src.you {
    color: var(--text);
  }
  .m .src.cron {
    color: var(--warn);
  }
  .m .src.gotify {
    color: var(--teal);
  }
  .m .src.slack {
    color: var(--ok);
  }
  .m .src.peer {
    color: var(--blue);
  }
  .m .nx {
    color: var(--accent);
  }
  .m .pos {
    margin-left: auto;
  }
</style>
