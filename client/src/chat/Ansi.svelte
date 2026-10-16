<script lang="ts">
  import { ansiRuns } from './ansi';

  /**
   * A command's own output, drawn with the colours it wrote.
   *
   * **One renderer for every terminal box**, so a running call's tail, a
   * settled call's tail and a foreground call's result cannot draw the same
   * bytes two ways. The runs wear the sheet's classes, which is where the
   * palette lives; a run with no class draws in the box's own tone, and every
   * sequence the page cannot draw is already gone by the time the runs are
   * built.
   */
  let { text }: { text: string } = $props();

  const runs = $derived(ansiRuns(text));
</script>

<!-- One line: the boxes draw with `white-space: pre-wrap`, so whitespace
     between these tags would be whitespace on screen. -->
{#each runs as run, at (at)}{#if run.cls === ''}{run.text}{:else}<span class={run.cls}
      >{run.text}</span
    >{/if}{/each}
