<script lang="ts">
  import Icon from '../components/Icon.svelte';
  import type { Connection } from '../socket';
  import type { QueuedPromptRow } from '../session/wire';
  import type { SessionSlot } from '../wire/types';

  /**
   * The prompts waiting in the CLI's queue, drawn as a stack above the box.
   *
   * **The pile is the core's, not this view's.** The seat read carries it and
   * `prompt_queued` / `prompt_lifecycle` keep it current, so two clients agree
   * about what is waiting; this draws it and dispatches a cancel. A row leaves
   * when the core says it left - `started` and the terminal states - so a
   * prompt the CLI already took cannot be drawn as dropped.
   *
   * **One row per prompt, and the depth is edges.** The newest sits fully
   * drawn at the bottom, against the box; every older prompt is one 6px arc
   * above it, so a deep queue costs a little height rather than a list. The
   * words are on the drawn row only.
   */
  let {
    rows,
    slot,
    connection,
  }: { rows: QueuedPromptRow[]; slot: SessionSlot; connection: Connection } = $props();

  /** The row being walked, by uuid: null is the box. */
  let cursor = $state<string | null>(null);

  /**
   * The walk's own row, which is none once it has left the pile.
   *
   * The cursor is held by uuid rather than by index, so a row the core settles
   * - delivered or dropped - takes the highlight with it instead of leaving it
   * on whichever row slid into its place.
   */
  const active = $derived(
    cursor !== null && rows.some((row) => row.uuid === cursor) ? cursor : null,
  );

  /**
   * The walk, as the terminal taught it: up enters at the newest and goes
   * older until the first queued prompt, down comes back, and one more down
   * past the newest lands in the box.
   */
  function move(direction: 'up' | 'down'): void {
    if (rows.length === 0) return;
    if (cursor === null) {
      if (direction === 'up') cursor = rows[rows.length - 1]?.uuid ?? null;
      return;
    }
    const at = rows.findIndex((row) => row.uuid === cursor);
    if (at < 0) return;
    if (direction === 'up') {
      cursor = rows[at - 1]?.uuid ?? cursor;
      return;
    }
    cursor = rows[at + 1]?.uuid ?? null;
  }

  function cancel(uuid: string): void {
    // Fire-and-forget like the composer's send: the answer rides the stream,
    // and the row leaves when the core says it left - `cancelled: false` means
    // the CLI had already taken it, and its own `started` frame settles it.
    void connection.dispatch({ cancel_queued_prompt: { key: slot, uuid } });
  }

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
    if ((event.key === 'Backspace' || event.key === 'Delete') && active !== null) {
      event.preventDefault();
      cancel(active);
      return;
    }
    if (event.key === 'Escape') {
      cursor = null;
    }
  }
</script>

{#if rows.length > 0}
  <div
    class="pile"
    tabindex="0"
    role="listbox"
    aria-label="Queued prompts: {rows.length} waiting"
    aria-activedescendant={active ?? ''}
    onkeydown={onkey}
  >
    <div class="head">
      <span>queued <b>{rows.length}</b></span>
      <span class="hint"><b>↑↓</b> walk &middot; <b>⌫</b> cancel</span>
    </div>
    <div class="rows">
      {#each rows as row, at (row.uuid)}
        {@const isCursor = active === row.uuid}
        {@const depth = rows.length - 1 - at}
        <div
          class="row"
          class:back={depth > 0}
          class:cur={isCursor}
          role="option"
          id={row.uuid}
          aria-selected={isCursor}
          style="transform: translateY({-depth * 6}px)"
        >
          <span class="w">{row.text}</span>
          <span class="m">
            <span class="src {row.source}">{row.source}</span>
            {#if at === 0}<span class="nx">next</span>{/if}
            {#if isCursor}
              <button class="del" type="button" onclick={() => cancel(row.uuid)}>
                <Icon name="x" class="ic" /> cancel
              </button>
            {/if}
          </span>
        </div>
      {/each}
    </div>
  </div>
{/if}

<style>
  /* The pile owns its height: one drawn row plus one 6px step per older
     prompt, so the column above shrinks by exactly that much. */
  .pile {
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
  .head .hint {
    margin-left: auto;
  }
  .rows {
    position: relative;
    padding-top: 6px;
  }
  .row {
    position: relative;
    border: 1px solid var(--line);
    border-radius: var(--r);
    background: var(--s2);
    padding: 8px 13px 7px;
    margin-top: -6px;
  }
  /* An arc is a surface: every row behind the cursor keeps its words to
     itself, so nothing bleeds through the step. */
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
  .m .src.you { color: var(--text); }
  .m .src.cron { color: var(--warn); }
  .m .src.gotify { color: var(--teal); }
  .m .src.slack { color: var(--ok); }
  .m .src.peer { color: var(--blue); }
  .m .nx { color: var(--accent); }
  .del {
    margin-left: auto;
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
  .del:hover,
  .del:focus-visible {
    color: var(--bad);
    border-color: var(--bad);
  }
  .pile:focus-visible .head {
    color: var(--muted);
  }
</style>
