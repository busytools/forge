<script lang="ts">
  import Icon from '../components/Icon.svelte';
  import { duration } from './numbers';
  import { reveal, subagents } from './subagents.svelte';

  /**
   * The sub-agent segment of the strip: how many instances the seat has out,
   * and the way into each one.
   *
   * **The segment is the only home subagents have outside the chat.** It reads
   * the same card join every dispatch row reads, so the count, the list and
   * the rows can never disagree. Hovering opens the list, a tap toggles it,
   * and a row's click reveals that dispatch's chat row - opened, scrolled to,
   * and flashed - rather than drawing a second copy of the instance anywhere.
   */

  /** Whether the list is open: hover, tap and focus all ask for it. */
  let open = $state(false);

  const all = $derived(subagents.all());
  const running = $derived(all.filter((card) => card.running));
  const settled = $derived(all.length - running.length);

  const agents = (count: number) => `${count} ${count === 1 ? 'agent' : 'agents'}`;

  /** The most important thing the summary can say, never a full tally. */
  const label = $derived(
    running.length > 0
      ? `${agents(running.length)} running`
      : `${agents(settled)} settled`,
  );
</script>

{#if all.length > 0}
  <span
    class="sg-seg"
    class:open
    onmouseenter={() => (open = true)}
    onmouseleave={() => (open = false)}
    onfocusout={() => (open = false)}
  >
    <button type="button" class="sg-tog" aria-expanded={open} onclick={() => (open = !open)}>
      {#if running.length > 0}
        <span class="ring"></span>
      {:else}
        <Icon name="check" class="ok" />
      {/if}
      {label}
    </button>

    {#if open}
      <div class="sg-list">
        {#each all as card (card.dispatch_id)}
          <button
            type="button"
            class="sg-it"
            onclick={() => {
              if (reveal(card.dispatch_id)) open = false;
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
            <span class="tx"
              >{card.running
                ? (card.tail[card.tail.length - 1]?.title ?? 'working')
                : card.name}</span
            >
            {#if card.usage !== null}
              <span class="n">{duration(card.usage.duration_ms)}</span>
            {/if}
          </button>
        {/each}
      </div>
    {/if}
  </span>
{/if}
