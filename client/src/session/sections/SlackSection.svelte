<script lang="ts">
  import type { SlackView } from '../view';
  import Section from './Section.svelte';

  /**
   * The slack section: one row per workspace, and each workspace's
   * subscriptions under it.
   *
   * **The nesting is a block, not a string prefix.** The terminal drew a
   * subscription as two `&nbsp;` in front of its target, a space character
   * standing in for a level of hierarchy: nothing could style it apart from
   * the workspace row, it could not wrap as a unit, and a space is not a
   * layout step. Here the subscriptions are a list of their own inside the
   * workspace's item, which is what the section is actually saying.
   */
  let { view }: { view: SlackView } = $props();
</script>

<Section name="slack" icon="slack" summary={view.summary}>
  <ul class="ws">
    {#each view.workspaces as workspace (workspace.name)}
      <li>
        <div class="kv">
          <span class="k">{workspace.name}</span>
          <span class="v">{workspace.connected ? 'connected' : 'not connected'}</span>
        </div>
        {#if workspace.subs.length > 0}
          <ul class="subs">
            {#each workspace.subs as sub (sub.id)}
              <li class="kv"><span class="k">{sub.k}</span><span class="v">{sub.v}</span></li>
            {/each}
          </ul>
        {/if}
      </li>
    {/each}
  </ul>
</Section>
