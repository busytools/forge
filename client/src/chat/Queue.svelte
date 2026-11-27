<script lang="ts">
  import { fade } from 'svelte/transition';

  import { subjectKey } from '../protocol';
  import { CONNECTION_DOWN } from '../refusals';
  import type { Connection } from '../socket';
  import type { QueueEnding, QueuedPromptRow } from '../session/wire';
  import { mintPromptId } from '../wire/ids';
  import type { SessionSlot } from '../wire/types';
  import { runningAt } from './conversation';
  import { echoes } from './echoes.svelte';
  import { wantsMotion } from './glide';
  import { appear, drawnAt } from './motion';
  import { renderInlineProse } from './prose';
  import {
    drawnRows,
    endingLine,
    endingRows,
    faceAt,
    noWordYet,
    retryable,
    walk,
    type DrawnRow,
  } from './queue';
  import { firstLine, joinedLine } from './text';
  import { inbound } from './units';

  /**
   * The prompts waiting in the CLI's queue, drawn as a stack above the box.
   *
   * **The pile is the core's, not this view's.** The seat read carries it and
   * `prompt_queued` / `prompt_lifecycle` keep it current, so two clients agree
   * about what is waiting; this draws it and dispatches a cancel. A row leaves
   * when the core says it left - `sent`, with its own row now in the chat, or
   * `not_sent`, with this pile's ending line carrying the reason - so a prompt
   * the CLI already took cannot be drawn as dropped.
   *
   * **This client's own press joins that pile.** A send the core has not
   * answered is a `sending` card here and nowhere else, so the words have one
   * home from the keypress to the chat row.
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

  /** The seat this pile belongs to, keyed the way every pending send is. */
  const seat = $derived(subjectKey({ session: slot }));

  /**
   * This client's own sends, which the pile draws with the core's.
   *
   * **The press lives here until the core answers it.** A send the core has
   * not listed yet is a `sending` card; one that never went is an ending line
   * with the reason and the retry the reader came back for.
   */
  const pending = $derived(echoes.of(seat));
  const drawn = $derived(drawnRows(rows, pending));
  const refused = $derived(endingRows(rows, pending));

  /** The one line the record's ending leaves, where the last card was. */
  const line = $derived(endingLine(ended, endingWords));

  /**
   * The clock the 30 s bound reads.
   *
   * Ticking only while a sending card is waiting: a pile of the core's own
   * rows has no bound to report, and a page idling on one should not hold a
   * timer open for it.
   */
  let now = $state(Date.now());
  $effect(() => {
    if (!drawn.some((row) => row.state === 'sending')) return;
    const beat = setInterval(() => (now = Date.now()), 1000);
    return () => clearInterval(beat);
  });

  /**
   * The words an ending carries, as plain text.
   *
   * **A peer envelope draws its body alone**, the same rule the card uses -
   * the `[Message id=...]` head is plumbing - and the line is rendered as
   * text rather than markup, so it takes the body's prose unrendered.
   */
  function endingWords(text: string): string {
    const envelope = inbound(text, null);
    return joinedLine(envelope?.kind === 'peer' ? firstLine(envelope.card.body) : text);
  }

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
   * the idle send: `queued` and `sent` land inside one beat (measured
   * 1-5 ms), so its card is up for about a frame and the eye mostly meets the
   * chat row the drain draws.
   */
  const FACE_HEIGHT = 64;

  /** The row being walked, by uuid: null is the box. */
  let cursor = $state<string | null>(null);

  let pile = $state<HTMLElement | null>(null);

  /**
   * Every card's position on the pass just drawn.
   *
   * **Read while the card is there**, because the chat row it becomes is
   * updated by another store in the same paint: a position recorded on the way
   * out could run after the row has already mounted. A card that stays put is
   * re-measured each pass, so its leaving pass has the position from the one
   * before.
   */
  $effect(() => {
    for (const row of drawn) {
      const node = pile?.querySelector<HTMLElement>(`[id="${row.uuid}"]`);
      if (node !== null && node !== undefined) drawnAt(row.uuid, node.getBoundingClientRect());
    }
  });

  /**
   * The walk's own row, which is none once it has left.
   *
   * Derived rather than written back: the core can settle the pointed row -
   * delivered or dropped - and a cursor naming it draws nothing, so the
   * highlight and the walk's next step both read the row that still exists.
   */
  const active = $derived(
    cursor !== null && drawn.some((row) => row.uuid === cursor) ? cursor : null,
  );

  /** Where the face sits: the cursor's row, or the newest when the walk is in the box. */
  const face = $derived(faceAt(active, drawn));

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
  function cardSource(row: DrawnRow): { label: string; kind: string } {
    const envelope = inbound(row.text, null);
    if (envelope?.kind === 'peer') return { label: envelope.card.peer, kind: 'peer' };
    return { label: row.source, kind: row.source };
  }

  /**
   * The card's own status: the one vocabulary, said in the row's line.
   *
   * `sending` is this client's word until the core answers; `queued` is the
   * core's; `no word yet` is the 30 s bound's uncertainty, never a failure.
   */
  function cardStatus(row: DrawnRow): string | null {
    if (row.state === 'queued') return null;
    if (noWordYet(row, now)) return 'no word yet';
    return row.state === 'sending' ? 'sending' : null;
  }

  /**
   * The walk, as the view this client replaced taught it: up enters at the
   * newest and goes older until the first queued prompt, down comes back, and
   * one more down past the newest lands in the box - focus included, so the
   * next Enter is the next send.
   */
  function move(direction: 'up' | 'down'): void {
    const was = active;
    cursor = walk(active, drawn, direction);
    if (cursor === null && was !== null && direction === 'down') {
      draft()?.focus();
    }
  }

  /**
   * Send the words again, from the line that says they did not go.
   *
   * The same dispatch the composer makes: a fresh id, a fresh pending row in
   * the pile, and the words carried exactly as they were. A refusal the first
   * attempt met is not inherited - the retry is a new send and starts its own
   * 30 s, because nothing about the first attempt's silence says this one
   * will be silent too.
   */
  function retry(words: string, source: string): void {
    if (words === '') return;
    const uuid = mintPromptId();
    echoes.remember(seat, uuid);
    const dispatch = echoes.mint();
    echoes.post(seat, uuid, words, runningAt(connection, slot, false), dispatch, source);
    try {
      void connection.dispatch(
        {
          prompt_under: {
            key: slot,
            text: words,
            attachments: [],
            uuid,
            source,
            dispatch_id: dispatch,
          },
        },
        slot,
      );
    } catch {
      echoes.refuse(seat, uuid, CONNECTION_DOWN);
    }
  }

  function cancel(uuid: string): void {
    // **A send the core has not listed has nothing to cancel there.** The
    // pending card is this client's, so it is this client's to take back.
    if (!rows.some((row) => row.uuid === uuid)) {
      echoes.clear(seat, uuid);
      return;
    }
    // Fire-and-forget like the composer's send: the answer rides the stream,
    // and the row leaves when the core says it left - `cancelled: false` means
    // the CLI had already taken it, and its own `sent` frame settles it.
    void connection.dispatch({ cancel_queued_prompt: { key: slot, uuid } }, slot);
  }

  /** The box this pile hands the keyboard back to, on the page both live under. */
  function draft(): HTMLTextAreaElement | null {
    return holder?.querySelector<HTMLTextAreaElement>('textarea') ?? null;
  }

  /** The composer bar, kept so the box is still reachable after the pile itself unmounts. */
  let holder: HTMLElement | null = null;
  let focused = $state(false);

  /**
   * Entering the pile - the empty box's up, or a click - lands on the newest,
   * which is where the walk's next up goes older from.
   */
  function onfocus(): void {
    holder = pile?.closest('.session-page')?.querySelector<HTMLElement>('.composer') ?? null;
    focused = true;
    if (active === null && drawn.length > 0) {
      cursor = drawn[drawn.length - 1]?.uuid ?? null;
    }
  }

  /**
   * The pile unmounting while it holds the keyboard hands it back to the box:
   * the last row leaving would otherwise drop focus on the body, and the next
   * keystroke with it.
   */
  $effect(() => {
    if (drawn.length === 0 && focused) {
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

{#if drawn.length > 0}
  <div class="pile">
    <!-- The head is OUTSIDE the listbox: a listbox holds options and nothing
         else, so the cancel control - the mouse's way to the same act as
         backspace - sits beside it rather than inside an option. -->
    <div class="qcount">
      <span>queued <b>{drawn.length}</b></span>
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
        <!-- Keyed on the position: a uuid is unique only while the core keeps
             it so, and a duplicate arriving must draw, not throw. -->
        {#each drawn as row, at (at)}
          {@const depth = face - at}
          {@const src = cardSource(row)}
          {@const status = cardStatus(row)}
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
              use:appear
            >
              <!-- The card's rendered line, which the module produced from
                   escaped input: the same renderer a row's preview uses. -->
              <!-- eslint-disable-next-line svelte/no-at-html-tags -->
              <span class="w">{@html cardWords(row.text)}</span>
              <span class="m">
                <span class="src {src.kind}">{src.label}</span>
                {#key status}
                  {#if status !== null}
                    <span class="st" transition:fade={{ duration: wantsMotion() ? 140 : 0 }}
                      >{status}</span
                    >
                  {/if}
                {/key}
                {#if at === 0}<span class="nx">next</span>{/if}
                {#if active === row.uuid}<span class="pos">#{at + 1} / {drawn.length}</span>{/if}
              </span>
            </div>
          {/if}
        {/each}
      </div>
    </div>
    {#each refused as ending, at (`refused-${at}`)}
      {#if retryable(ending.source)}
        <div class="ended">
          {endingLine(ending, endingWords)}
          <button class="retry" type="button" onclick={() => retry(ending.text, ending.source)}>
            retry
          </button>
        </div>
      {:else}
        <div class="ended">{endingLine(ending, endingWords)}</div>
      {/if}
    {/each}
    {#if line !== null}
      <div class="ended">
        {line}
        {#if ended !== null && retryable(ended.source)}
          <button class="retry" type="button" onclick={() => retry(ended.text, ended.source)}>
            retry
          </button>
        {/if}
      </div>
    {/if}
  </div>
{:else if line !== null || refused.length > 0}
  <!-- The queue is empty and a card left with a word: the line stands where
       the pile was, because a row that vanishes silently reads as one that was
       delivered. -->
  {#each refused as ending, at (`refused-${at}`)}
    {#if retryable(ending.source)}
      <div class="ended">
        {endingLine(ending, endingWords)}
        <button class="retry" type="button" onclick={() => retry(ending.text, ending.source)}>
          retry
        </button>
      </div>
    {:else}
      <div class="ended">{endingLine(ending, endingWords)}</div>
    {/if}
  {/each}
  {#if line !== null}
    <div class="ended">
      {line}
      {#if ended !== null && retryable(ended.source)}
        <button class="retry" type="button" onclick={() => retry(ended.text, ended.source)}>
          retry
        </button>
      {/if}
    </div>
  {/if}
{/if}

<style>
  /* The pile owns its height: the face plus one step per row above it, so the
     column above shrinks by exactly that much and nothing paints over it. */
  .qlist {
    outline: none;
  }
  /* **The pile sits at the conversation's foot.** No seam above it: the
     header meets the last chat row, where the block gap this used to carry
     read as a row of its own (Ved, 2026-11-03). Its height stays its own, so
     the column above shrinks by exactly that much and nothing paints over
     it, and the gap below it to the box stays. */
  .pile {
    margin-top: 0;
    margin-bottom: 8px;
  }
  .ended {
    padding: 2px 0 0 11px;
    font-family: var(--mono);
    font-size: var(--fs-label);
    color: var(--dim);
    display: flex;
    align-items: baseline;
    gap: 8px;
  }
  /* The card's own state, in the meta's own face: the one vocabulary, said
     where the card already carries its sender. */
  /* The state crosses in place: a reserved width means `sending` becoming
     `no word yet` never walks the chips beside it. */
  .m .st {
    color: var(--warn);
    min-width: 11ch;
  }
  /* The refusal's retry, where the words are: a small control on the ending
     line, never a second surface. */
  .retry {
    margin-left: auto;
    padding: 0;
    border: 0;
    background: none;
    font: inherit;
    color: var(--accent);
    text-decoration: underline;
    cursor: pointer;
  }
  /* Touch reaches every control: the retry is a finger's target too, so it
     takes the 44 px minimum under a coarse pointer. */
  @media (pointer: coarse) {
    .retry {
      min-height: 44px;
    }
  }
  /* The header's pieces sit together: a right-pinned hint is a step in the
     mock's tile and a canyon in the real column (#1705). */
  .qcount {
    display: flex;
    align-items: baseline;
    gap: 9px;
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
