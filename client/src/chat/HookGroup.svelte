<script lang="ts">
  import { untrack } from 'svelte';

  import Chevron from '../components/Chevron.svelte';
  import Icon from '../components/Icon.svelte';
  import Hook from './Hook.svelte';
  import type { HookGroupLeaf } from './units';

  /**
   * A session start's hooks as one row: what fired, how many hooks it carried,
   * and the hooks themselves behind the row's own open.
   *
   * **One row per firing, because the burst is one event.** Eleven rows at the
   * head of a fresh session buried the turn's first work, and several of them
   * were empty (Ved, 2026-10-24). The firing is the row; the hooks are what it
   * opens onto.
   *
   * **A hook that finished without a word draws no row of its own** - that
   * empty line is what the reader met - while one that FAILED or is still
   * running stays even silent: the failure is the whole of what it has to say,
   * and a running hook's name is what says it is out. A failure never rides
   * the glyph's colour alone: the label counts the hooks that failed, so a
   * reader who cannot see the tone still reads the state closed.
   *
   * Nothing is dropped (rule 25): every hook's output stays reachable inside
   * the row it belongs to, and the count says how many the firing carried.
   */
  let { group, open = false }: { group: HookGroupLeaf; open?: boolean } = $props();

  /** Whether the row is open; a closed one carries its summary and nothing else. */
  let opened = $state(untrack(() => open));

  /** The event in words: `SessionStart` reads as `session start`. */
  const event = $derived(group.event.replace(/([a-z])([A-Z])/g, '$1 $2').toLowerCase());

  /** The hooks that failed, whose count the closed row carries in words. */
  const failed = $derived(group.runs.filter((held) => held.run.failed).length);

  /** What fired, how many hooks it carried and how many failed, closed. */
  const label = $derived(
    `${event} \u{b7} ${group.runs.length} ${group.runs.length === 1 ? 'hook' : 'hooks'}` +
      (failed > 0 ? ` \u{b7} ${failed} failed` : ''),
  );

  /**
   * The hooks a reader has something to see in: those that printed something,
   * any that failed, and any still running - a silent failure's state is the
   * row itself, and a running hook has not had its chance to speak yet.
   *
   * **Emptiness is the raw output's, not the tail's.** A body of nothing but
   * one block mark strips to an empty tail but is still output, and hiding it
   * would drop what the hook printed.
   */
  const shown = $derived(
    group.runs.filter(
      (held) =>
        !held.run.settled ||
        held.run.failed ||
        (held.run.body !== null && held.run.body.trim() !== ''),
    ),
  );
</script>

<details class="leaf hookgroup" bind:open={opened}>
  <summary>
    <Icon name="hook" class={`gl${failed > 0 ? ' err' : ' ok'}`} />
    <span class="tn">{label}</span>
    <Chevron />
  </summary>
  {#if opened}
    <div class="body">
      {#each shown as held (held.key)}
        <Hook run={held.run} />
      {:else}
        <div class="dim">the hooks printed nothing</div>
      {/each}
    </div>
  {/if}
</details>
