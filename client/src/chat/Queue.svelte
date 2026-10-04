<script lang="ts">
  import type { Connection } from '../socket';
  import type { QueueEnding, QueuedPromptRow } from '../session/wire';
  import type { SessionSlot } from '../wire/types';
  import { renderInlineProse } from './prose';
  import { endingLine, faceAt, walk } from './queue';
  import { firstLine, joinedLine } from './text';
  import { inbound } from './units';

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
   * the pile's foot and lifted by one step per queued prompt, so the rows
   * behind the face show as edges rather than as a list: the height the pile
   * occupies is the face plus those steps, and the chat column above shrinks
   * by exactly that much. The face is whichever row the walk is on - the
   * newest when the walk is in the box - and only the face draws its words.
   * **The step is wide on purpose** (Ved, 2026-10-04): at six pixels the
   * edges read as one smudge, so every queued prompt shows a strip of its own
   * card.
   */
  let {
    rows,
    ended = null,
    slot,
    connection,
  }: {
    rows: QueuedPromptRow[];
    ended?: QueueEnding | null;
    slot: SessionSlot;
    connection: Connection;
  } = $props();

  /** The one line an ending leaves, where the last card was. */
  const line = $derived(endingLine(ended));

  /** The step between one arc and the next. */
  const STEP = 16;

  /**
   * One row's drawn height, which the container's own height is built from.
   *
   * A constant rather than a measurement, because the app ships ONE type
   * scale (`web.css`): the row is 8+7 of padding, a 1.5-line of prose, 5 of
   * gap, a 1.5-line of mono meta and a hairline, and every row is the same by
   * construction - the words never wrap. Measuring would tie the pile to a
   * `ResizeObserver` for a number the theme already fixes.
   *
   * **The arithmetic has no cap**: a pile deeper than the room draws deeper
   * than the room, on purpose - a cap would hide waiting prompts, which is
   * the opposite of what the pile is for. The other edge of the same coin is
   * the idle send: `queued` and `started` land inside one beat (measured
   * 1-5 ms), so its card is up for about a frame and the eye mostly meets the
   * chat row the drain draws.
   */
  const FACE_HEIGHT = 64;

  /** The row being walked, by uuid: null is the box. */
  let cursor = $state<string | null>(null);

  let pile = $state<HTMLElement | null>(null);

  /**
   * The walk's own row, which is none once it has left.
   *
   * Derived rather than written back: the core can settle the pointed row -
   * delivered or dropped - and a cursor naming it draws nothing, so the
   * highlight and the walk's next step both read the row that still exists.
   */
  const active = $derived(
    cursor !== null && rows.some((row) => row.uuid === cursor) ? cursor : null,
  );

  /** Where the face sits: the cursor's row, or the newest when the walk is in the box. */
  const face = $derived(faceAt(active, rows));

  /**
   * The words the card draws, as one rendered line.
   *
   * **A peer envelope draws its body alone** (Ved, 2026-10-04): the raw
   * `[Message id=...]` head is plumbing, and who sent it is already the meta
   * row's own word - a different lead shows its project, a worker its
   * `project/label` - so the text says nothing the row below it does not. Any
   * other prompt is the reader's own and renders the same way.
   */
  function cardWords(text: string): string {
    const envelope = inbound(text, null);
    if (envelope?.kind === 'peer') {
      return renderInlineProse(joinedLine(firstLine(envelope.card.body)));
    }
    return renderInlineProse(joinedLine(text));
  }

  /**
   * The meta's source chip: the sender for a peer envelope, the kind otherwise.
   *
   * **A peer envelope already names its sender** (`project` for a lead,
   * `project/label` for a worker), and that is what tells a reader what the
   * message IS - so the chip carries it, coloured as peer traffic. Every
   * other prompt has no other sender to name, and its kind is its own word.
   */
  function cardSource(row: QueuedPromptRow): { label: string; kind: string } {
    const envelope = inbound(row.text, null);
    if (envelope?.kind === 'peer') return { label: envelope.card.peer, kind: 'peer' };
    return { label: row.source, kind: row.source };
  }

  /**
   * The walk, as the terminal taught it: up enters at the newest and goes
   * older until the first queued prompt, down comes back, and one more down
   * past the newest lands in the box - focus included, so the next Enter is
   * the next send.
   */
  function move(direction: 'up' | 'down'): void {
    const was = active;
    cursor = walk(active, rows, direction);
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
    if (active === null && rows.length > 0) {
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
    if ((event.key === 'Backspace' || event.key === 'Delete') && active !== null) {
      event.preventDefault();
      cancel(active);
      return;
    }
    if (event.key === 'Escape') {
      // Back to the box, keyboard included: the contract the walk advertises.
      cursor = null;
      draft()?.focus();
    }
  }
</script>

{#if rows.length > 0}
  <div class="pile">
    <!-- The head is OUTSIDE the listbox: a listbox holds options and nothing
         else, so the cancel control - the mouse's way to the same act as
         backspace - sits beside it rather than inside an option. -->
    <div class="qcount">
      <span>queued <b>{rows.length}</b></span>
      <span class="qhint"
        ><b>↑↓</b> walk{#if active !== null}
          &middot; <b>⌫</b> cancel{/if}</span
      >
    </div>
    <div
      class="qlist"
      bind:this={pile}
      tabindex="-1"
      role="listbox"
      aria-label="Queued prompts"
      aria-activedescendant={active ?? ''}
      {onfocus}
      onblur={() => (focused = false)}
      onkeydown={onkey}
      onclick={() => pile?.focus()}
    >
      <div class="qstack" style="height: {FACE_HEIGHT + face * STEP}px">
        {#each rows as row, at (row.uuid)}
          {@const depth = face - at}
          {@const src = cardSource(row)}
          {#if depth >= 0}
            <div
              class="qcard"
              class:back={depth > 0}
              class:cur={active === row.uuid}
              role="option"
              id={row.uuid}
              tabindex="-1"
              aria-selected={active === row.uuid}
              style="transform: translateY({-depth * STEP}px)"
            >
              <!-- The card's rendered line, which the module produced from
                   escaped input: the same renderer a row's preview uses. -->
              <!-- eslint-disable-next-line svelte/no-at-html-tags -->
              <span class="w">{@html cardWords(row.text)}</span>
              <span class="m">
                <span class="src {src.kind}">{src.label}</span>
                {#if at === 0}<span class="nx">next</span>{/if}
                {#if active === row.uuid}<span class="pos">#{at + 1} / {rows.length}</span>{/if}
              </span>
            </div>
          {/if}
        {/each}
      </div>
    </div>
    {#if line !== null}
      <div class="ended">{line}</div>
    {/if}
  </div>
{:else if line !== null}
  <!-- The queue is empty and the last card left with a word: the line stands
       where the pile was, because a row that vanishes silently reads as one
       that was delivered. -->
  <div class="ended">{line}</div>
{/if}

<style>
  /* The pile owns its height: the face plus one step per row above it, so the
     column above shrinks by exactly that much and nothing paints over it. */
  .qlist {
    outline: none;
  }
  /* **The pile separates itself from the turn above, and its box below.**
     The composer's own margin pulls it up under the conversation
     (`margin-top: calc(-1 * var(--ins))`, written for the box alone); with
     the pile on top the card met the last row flush (#1705), so the pile
     carries the block gap instead - and the mock gives the box its own
     `margin-top: 8px`, which is the seam the last card was missing below. */
  .pile {
    margin-top: 8px;
    margin-bottom: 8px;
  }
  .ended {
    padding: 2px 0 0 11px;
    font-family: var(--mono);
    font-size: var(--fs-label);
    color: var(--dim);
  }
  .qcount {
    display: flex;
    align-items: baseline;
    gap: 10px;
    font-family: var(--mono);
    font-size: var(--fs-label);
    color: var(--dim);
    margin-bottom: 5px;
  }
  .qcount b {
    color: var(--muted);
    font-weight: 400;
  }
  /* No cancel control in the head (Ved, 2026-10-04): its appearing and
     vanishing moved everything beside it with every step of the walk, and the
     hint below already says which key does it. */
  /* The header's pieces sit together rather than pinned to the column's ends:
     the mock's tile is 430px, where a right-pinned hint is a step; the real
     column is three times that, where the same rule is a canyon (#1705, and
     the mock is the reference the divergence is named against). */
  .qcount .qhint {
    margin-left: 0;
  }
  .qstack {
    position: relative;
  }
  .qcard {
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
  .qcard.back .w,
  .qcard.back .m {
    visibility: hidden;
  }
  .qcard.cur {
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
  .qcard.cur .w {
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
  /* Forge's own deliveries, which are neither the reader's nor a connector's. */
  .m .src.forge {
    color: var(--violet);
  }
  .m .nx {
    color: var(--accent);
  }
  .m .pos {
    margin-left: auto;
  }
</style>
