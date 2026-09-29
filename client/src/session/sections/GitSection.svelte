<script lang="ts">
  import type { GitView } from '../view';
  import Section from './Section.svelte';

  /**
   * The git section: the branch the seat's tree is on and what moved in it,
   * and the pull request that tree belongs to.
   *
   * It is the inspector's first section because the working tree is the first
   * thing a person looks for, and because it is the one read whose absence
   * reads as "this seat has no repository" rather than as a read that did not
   * happen.
   */
  let { view }: { view: GitView } = $props();
</script>

<Section name="git" icon="git" summary={view.summary} open={view.open}>
  {#if view.pr !== null}
    <div class="kv">
      <span class="k">PR #{view.pr}</span>
      {#if view.closes !== null}<span class="v a">{'\u{2192} closes '}{view.closes}</span>{/if}
    </div>
  {/if}
  {#if view.gate !== null}
    <div class="kv"><span class="k">{view.gate}</span></div>
  {/if}
</Section>
