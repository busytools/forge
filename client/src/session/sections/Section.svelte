<script lang="ts">
  import type { Snippet } from 'svelte';

  import Chevron from '../../components/Chevron.svelte';
  import Icon from '../../components/Icon.svelte';

  /**
   * One inspector section: an icon, a name, a summary of what is behind it,
   * and the body it opens on.
   *
   * `data-k` is the sheet's own handle on a section - the schedules rules read
   * it - and it is the name the reader's open state is held against, which is
   * why it is the section's name rather than an index.
   */
  let {
    name,
    icon,
    summary = '',
    open: startsOpen = false,
    children,
  }: {
    name: string;
    icon: string;
    summary?: string;
    /** Whether the section opens on first draw. The reader's toggle outranks it after. */
    open?: boolean;
    children: Snippet;
  } = $props();

  // Held here rather than in the page: a live update redraws the inspector on
  // every frame, and a section whose open state came from its props would
  // close under the reader the moment the record moved. Read once, through a
  // call, because the initial value is the whole of what the prop is for.
  const initially = () => startsOpen;
  let open = $state(initially());
</script>

<details class="sec" bind:open data-k={`sec-${name}`}>
  <summary>
    <Icon name={icon} class="gl" />
    {name}
    {#if summary !== ''}<span class="c2">{summary}</span>{/if}
    <Chevron />
  </summary>
  {#if open}
    <div class="sb">{@render children()}</div>
  {/if}
</details>
