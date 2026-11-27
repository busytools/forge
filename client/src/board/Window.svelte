<script lang="ts">
  import { hrefForSlot } from '../routes';
  import type { SessionSlot } from '../wire/types';
  import Icon from '../components/Icon.svelte';
  import type { WindowView } from './window';
  import { clockOf } from './window';

  /**
   * The window: the board drawn for a person, exceptions first.
   *
   * Four regions in the order a reader needs them - what needs them, what
   * looks wrong, who is doing what, and what is next. Every fact is the
   * server's own: this component orders and words, it never derives.
   *
   * Everything the reader presses goes out through `onact`, the page's own
   * dispatch, and a row's id opens the drawer through `onopen`.
   */
  let {
    view,
    onact = null,
    onopen = null,
  }: {
    view: WindowView;
    onact?: ((command: Record<string, Record<string, unknown>>) => void) | null;
    onopen?: ((id: string) => void) | null;
  } = $props();

  /** The note a decision card sends with its choice, per row. */
  const notes: Record<string, string> = $state({});
  /** The words a look's send-back carries, per row. */
  const words: Record<string, string> = $state({});
  /** Whether a card's note box is open: a card reads as an ask, not a form. */
  const noteOpen: Record<string, boolean> = $state({});
  /** Whether a card's evidence is expanded: one summary line until asked. */
  const evidenceOpen: Record<string, boolean> = $state({});

  /** How much of the evidence a collapsed card shows before it is expanded. */
  const SUMMARY_CHARS = 90;

  function summaryOf(text: string): string {
    return text.length > SUMMARY_CHARS ? `${text.slice(0, SUMMARY_CHARS).trimEnd()}...` : text;
  }

  function act(command: Record<string, Record<string, unknown>>): void {
    onact?.(command);
  }

  /** The seat's page, where the discuss door opens the ask in the chat. */
  function discussHref(seat: SessionSlot): string {
    return hrefForSlot(seat);
  }

  /** The chip an evidence state draws; `null` for a row carrying none. */
  function evidenceChip(state: 'verified' | 'unchecked' | 'failed' | null): {
    label: string;
    tone: string;
  } | null {
    if (state === 'verified') return { label: 'evidence verified', tone: 'blue' };
    if (state === 'failed') return { label: 'evidence refused', tone: 'bad' };
    if (state === 'unchecked') return { label: 'evidence unchecked', tone: 'warn' };
    return null;
  }
</script>

<section class="w-window" aria-label="the board window">
  <section class="w-part" aria-labelledby="w-needs-h">
    <h2 id="w-needs-h" class="w-h">
      needs you {#if view.needs.length > 0}<span class="w-n">{view.needs.length}</span>{/if}
    </h2>
    {#if view.needs.length === 0}
      <p class="w-quiet">nothing is waiting on you</p>
    {:else}
      <div class="w-needs">
        {#each view.needs as card (card.id)}
          {@const chip = evidenceChip(card.evidenceState)}
          {@const expanded = evidenceOpen[card.id] === true}
          {@const note = noteOpen[card.id] === true}
          <article class="w-need" data-id={card.id}>
            <div class="w-meta">
              <span class="w-kind"
                >{#if card.kind === 'look'}<Icon name="check" /> a look{:else}decision{/if}</span
              >
              <span class="w-ctx"
                >{card.subject}{#if card.owner !== null}
                  &middot; {card.owner}{/if}</span
              >
              <span class="w-age">waiting {card.age}</span>
            </div>
            <h3 class="w-q">{card.question}</h3>
            {#if card.artifact !== null}
              <a class="w-door" href={card.artifact.href} target="_blank" rel="noreferrer"
                >look at <span class="nm">{card.artifact.label}</span></a
              >
            {/if}
            {#if chip !== null || card.evidence !== null}
              <div class="w-ev" class:open={expanded}>
                {#if chip !== null}<span class="b-chip {chip.tone}">{chip.label}</span>{/if}
                {#if card.evidence !== null}
                  <span class="w-ev-text"
                    >{expanded ? card.evidence : summaryOf(card.evidence)}</span
                  >
                  {#if card.evidence.length > SUMMARY_CHARS}
                    <button
                      type="button"
                      class="w-more"
                      aria-expanded={expanded}
                      onclick={() => (evidenceOpen[card.id] = !expanded)}
                      >{expanded ? 'less' : 'more'}</button
                    >
                  {/if}
                {/if}
              </div>
            {/if}
            <!-- One action bar: filled primaries, an outlined secondary, and
                 the note as a ghost button. The row's meta doors are the
                 footer's, never this bar's. -->
            <div class="w-actions">
              {#if card.kind === 'decision'}
                {#if card.options.length > 0}
                  <div class="w-options" role="group" aria-label="answer with one of">
                    {#each card.options as option (option)}
                      <button
                        type="button"
                        class="w-primary"
                        onclick={() =>
                          act({
                            task_answer: {
                              project: view.project,
                              id: card.id,
                              choice: option,
                              words: notes[card.id] ?? '',
                            },
                          })}>{option}</button
                      >
                    {/each}
                  </div>
                {/if}
                {#if note}
                  <input
                    class="b-in w-note"
                    data-editor="board"
                    type="text"
                    placeholder={card.options.length > 0 ? 'note (optional)' : 'your answer'}
                    aria-label="the note sent with the answer"
                    bind:value={notes[card.id]}
                  />
                  {#if card.options.length === 0}
                    <button
                      type="button"
                      class="w-primary"
                      onclick={() =>
                        act({
                          task_answer: {
                            project: view.project,
                            id: card.id,
                            words: notes[card.id] ?? '',
                          },
                        })}>answer</button
                    >
                  {/if}
                {:else}
                  <button
                    type="button"
                    class="w-ghost"
                    aria-expanded={note}
                    onclick={() => (noteOpen[card.id] = true)}
                    >{card.options.length > 0 ? 'add a note' : 'answer in your words'}</button
                  >
                {/if}
              {:else}
                {#if card.approveBlocked !== null}
                  <p class="w-hold">{card.approveBlocked}</p>
                {:else}
                  <button
                    type="button"
                    class="b-btn b-ok w-primary"
                    onclick={() =>
                      act({
                        task_verdict: { project: view.project, id: card.id, approve: true },
                      })}>approve</button
                  >
                {/if}
                <button
                  type="button"
                  class="b-btn b-no w-secondary"
                  onclick={() =>
                    act({
                      task_verdict: {
                        project: view.project,
                        id: card.id,
                        approve: false,
                        words: words[card.id] ?? '',
                      },
                    })}>send back</button
                >
                {#if note}
                  <input
                    class="b-in w-note"
                    data-editor="board"
                    type="text"
                    placeholder="what to change"
                    aria-label="what to change (sent back with it)"
                    bind:value={words[card.id]}
                  />
                {:else}
                  <button
                    type="button"
                    class="w-ghost"
                    aria-expanded={note}
                    onclick={() => (noteOpen[card.id] = true)}>add a note</button
                  >
                {/if}
              {/if}
            </div>
            <div class="w-foot">
              {#if card.seat !== null}
                <a class="w-foot-link" href={discussHref(card.seat)}>discuss</a>
                <span class="w-dot" aria-hidden="true">&middot;</span>
              {/if}
              <button type="button" class="w-foot-link" onclick={() => onopen?.(card.id)}
                >open the row</button
              >
            </div>
          </article>
        {/each}
      </div>
    {/if}
  </section>

  <section class="w-part" aria-labelledby="w-exceptions-h">
    <h2 id="w-exceptions-h" class="w-h">
      exceptions {#if view.exceptions.length > 0}<span class="w-n">{view.exceptions.length}</span
        >{/if}
    </h2>
    {#if view.exceptions.length === 0}
      <p class="w-quiet">nothing contradicts the board</p>
    {:else}
      <ul class="w-list">
        {#each view.exceptions as item (item.id ?? `worker:${item.subject}`)}
          <li class="w-exc">
            {#if item.id !== null}
              <button type="button" class="w-link" onclick={() => onopen?.(item.id as string)}
                >{item.subject}</button
              >
            {:else}
              <span class="w-strong">{item.subject}</span>
            {/if}
            <span class="w-why">{item.line}</span>
          </li>
        {/each}
      </ul>
    {/if}
  </section>

  <section class="w-part" aria-labelledby="w-live-h">
    <h2 id="w-live-h" class="w-h">
      live {#if view.live.length > 0}<span class="w-n">{view.live.length}</span>{/if}
    </h2>
    {#if view.live.length === 0}
      <p class="w-quiet">no seats are open on this project</p>
    {:else}
      <ul class="w-list">
        {#each view.live as line (line.label)}
          <li class="w-live">
            <a class="w-who" href={discussHref(line.seat)}>{line.label}</a>
            {#if line.doing === null}
              <span class="w-why">holds nothing</span>
            {:else}
              <span class="w-strong">{line.doing}</span>
              {#if line.since !== null}
                <span class="w-why">since {clockOf(line.since)}</span>
              {:else}
                <span class="w-why">since unknown</span>
              {/if}
              {#if line.artifact !== null}
                <span class="w-why">{line.artifact}</span>
              {:else}
                <span class="w-why">nothing touched yet</span>
              {/if}
              <span class="w-why">{line.next === null ? 'no estimate' : `next ${line.next}`}</span>
              {#if line.held > 1}
                <span class="w-why">and {line.held - 1} more held</span>
              {/if}
            {/if}
          </li>
        {/each}
      </ul>
    {/if}
  </section>

  <section class="w-part" aria-labelledby="w-next-h">
    <h2 id="w-next-h" class="w-h">
      next {#if view.next.length > 0}<span class="w-n">{view.next.length}</span>{/if}
    </h2>
    {#if view.next.length === 0}
      <p class="w-quiet">the queue is empty</p>
    {:else}
      <ol class="w-list w-next">
        {#each view.next as row (row.id)}
          <li>
            <button type="button" class="w-link" onclick={() => onopen?.(row.id)}
              >{row.subject}</button
            >
            {#if row.standing}
              <span class="w-why"
                >standing{row.measure === null ? '' : ` \u{b7} every ${row.measure}`}</span
              >
            {:else if row.measure !== null}
              <span class="w-why">{row.measure}</span>
            {/if}
          </li>
        {/each}
      </ol>
    {/if}
  </section>
</section>
