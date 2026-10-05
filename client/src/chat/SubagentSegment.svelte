<script lang="ts">
  import Icon from '../components/Icon.svelte';
  import { askReveal } from '../session/scroll-ask';
  import { duration } from './numbers';
  import { latestFirst, subagents, transcribable } from './subagents.svelte';
  import { firstLine } from './text';

  /**
   * The agents row above the composer: how many instances the seat has out
   * and how many it has finished, and the way into each one.
   *
   * **Persistent where the strip is not.** The strip fades with the turn it
   * reports; this row is the seat's own agent work, so it stays for as long
   * as the session holds any. It reads the same card join every dispatch row
   * reads, so the counts, the list and the rows can never disagree.
   *
   * **Only instances a transcript can be opened for are listed.** The row a
   * dispatch drew is where its transcript lives - and an instance that ran
   * before a restart or resume has no frames on the page, so its row would
   * open onto a brief and nothing. Such instances stay out of the list rather
   * than leading somewhere empty.
   */
  const MAX_ACTIVITY = 64;

  /** Whether the list is open: hover, tap and focus all ask for it. */
  let open = $state(false);
  /** A beat of grace on leaving, so crossing the gap into the list lands. */
  let closing: ReturnType<typeof setTimeout> | null = null;

  // The counts and the list are the SAME set - the instances a transcript can
  // be opened for - so the numbers and the rows can never disagree.
  const listed = $derived(transcribable(subagents.all()));
  const running = $derived(listed.filter((card) => card.running));
  const finished = $derived(listed.length - running.length);
  // Read newest first: the instance a reader just watched start is the one
  // they came to the list for.
  const latest = $derived(latestFirst(listed));

  /** What one instance is doing, short enough for a row: one line, capped. */
  const doing = (card: (typeof listed)[number]) => {
    const said = firstLine(card.tail[card.tail.length - 1]?.title ?? 'working').trim();
    return said.length > MAX_ACTIVITY ? `${said.slice(0, MAX_ACTIVITY).trimEnd()}\u{2026}` : said;
  };

  function hold() {
    if (closing !== null) clearTimeout(closing);
    closing = null;
    open = true;
  }
  function release() {
    if (closing !== null) clearTimeout(closing);
    closing = setTimeout(() => {
      closing = null;
      open = false;
    }, 250);
  }
</script>

{#if listed.length > 0}
  <span class="sg-seg" class:open onmouseenter={hold} onmouseleave={release} onfocusout={release}>
    <button type="button" class="sg-tog" aria-expanded={open} onclick={() => (open = !open)}>
      <!-- The subagents glyph leads, so the segment reads as what it is; the
           mark after it is the state. -->
      <Icon name="subagents" />
      {#if running.length > 0}
        <span class="ring"></span>
      {:else}
        <Icon name="check" class="ok" />
      {/if}
      {running.length} running &#183; {finished} finished
    </button>

    {#if open}
      <div class="sg-list">
        {#each latest as card (card.dispatch_id)}
          <button
            type="button"
            class="sg-it"
            onclick={() => {
              askReveal(card.dispatch_id);
              open = false;
            }}
          >
            {#if card.running}
              <span class="ring"></span>
            {:else if card.failed}
              <Icon name="x" class="bad" />
            {:else}
              <Icon name="check" class="ok" />
            {/if}
            <span class="nm">{card.agent_type ?? 'agent'}</span>
            <span class="tx">{card.running ? doing(card) : card.name}</span>
            {#if card.usage !== null}
              <span class="n">{duration(card.usage.duration_ms)}</span>
            {/if}
          </button>
        {/each}
      </div>
    {/if}
  </span>
{/if}
