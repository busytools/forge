<script lang="ts">
  import type { GotifyView } from '../view';
  import Section from './Section.svelte';

  /**
   * The gotify section: the stream's liveness and what this project is
   * subscribed to on it.
   *
   * One row per subject rather than one per subscription: a delivery is let
   * through when ANY subscription matches, so the two facts are each read
   * across the whole set.
   */
  let { view }: { view: GotifyView } = $props();
</script>

<Section name="gotify" icon="gotify" summary={view.summary}>
  {#each view.rows as row (row.k)}
    <div class="kv"><span class="k">{row.k}</span><span class="v">{row.v}</span></div>
  {/each}
</Section>
