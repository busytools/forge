<script lang="ts">
  /**
   * A composer driven the way a reader drives it, for the states that only
   * exist while something is being typed.
   *
   * The in-flight slash command is one: the line that names it comes from the
   * text the READER sent, so no prop can put the composer in that state. This
   * types the command and presses Enter on mount, which is the same two events
   * a person produces - so the specimen is the real state rather than a copy of
   * what it is believed to look like.
   *
   * Test support for the by-width page: nothing the app ships imports it.
   */
  import { onMount } from 'svelte';

  import Composer from '../composer/Composer.svelte';
  import type { ComposerProps } from '../composer/view';

  let { props, command }: { props: ComposerProps; command: string } = $props();
  let box = $state<HTMLDivElement | null>(null);

  onMount(() => {
    const field = box?.querySelector('textarea');
    if (!(field instanceof HTMLTextAreaElement)) return;
    field.value = command;
    field.dispatchEvent(new Event('input', { bubbles: true }));
    // Twice, and that is the reader's own sequence: the first Enter takes the
    // row the list is offering - the command itself - and the second sends it.
    for (let press = 0; press < 2; press += 1) {
      field.dispatchEvent(
        new KeyboardEvent('keydown', { key: 'Enter', bubbles: true, cancelable: true }),
      );
    }
  });
</script>

<div class="comp" bind:this={box}>
  <Composer {...props} />
</div>
