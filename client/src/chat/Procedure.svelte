<script lang="ts">
  import { untrack } from 'svelte';

  import Chevron from '../components/Chevron.svelte';
  import Icon from '../components/Icon.svelte';
  import Prose from './Prose.svelte';

  /**
   * The handoff's own prompt, as the transcript carries it: the procedure that
   * opens the judgment turn in the session handing over, or the brief that
   * opens the successor's.
   *
   * **It is the turn's opening row, not a nudge inside one.** It carries the
   * handoff's own mark and names itself, because it says why the seat was
   * replaced; opened, it reads as the whole procedure the session was given.
   * The calls that follow it are the work it set going, drawn as any turn's
   * calls are.
   */
  let { text, open = false }: { text: string; open?: boolean } = $props();

  /** Whether the row is open; a closed row carries its name and nothing else. */
  let opened = $state(untrack(() => open));
</script>

<details class="leaf procedure" bind:open={opened}>
  <summary>
    <Icon name="handoff" class="gl" />
    <span class="k">handoff procedure</span>
    <Chevron />
  </summary>
  {#if opened}
    <div class="body">
      <Prose {text} />
    </div>
  {/if}
</details>
