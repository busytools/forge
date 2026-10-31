<script lang="ts">
  /**
   * A composer driven the way a reader drives it, for the states that only
   * exist while something is being typed or after a press.
   *
   * The in-flight slash command is one: the line that names it comes from the
   * text the READER sent, so no prop can put the composer in that state. This
   * types the text and, when `send` is on, presses Enter - the same events a
   * person produces. A menu's open state is the same machinery with `send` off,
   * and a folded ask is the press the reader makes, named by `click`.
   *
   * Test support for the by-width page: nothing the app ships imports it.
   */
  import { onMount, tick } from 'svelte';

  import Composer from '../composer/Composer.svelte';
  import type { ComposerProps } from '../composer/view';

  let {
    props,
    command = '',
    send = true,
    click = null,
  }: {
    props: ComposerProps;
    command?: string;
    send?: boolean;
    click?: string | null;
  } = $props();
  let box = $state<HTMLDivElement | null>(null);

  onMount(async () => {
    const field = box?.querySelector('textarea');
    if (field instanceof HTMLTextAreaElement && command !== '') {
      field.value = command;
      field.dispatchEvent(new Event('input', { bubbles: true }));
      // Twice, and that is the reader's own sequence: the first Enter takes
      // the row the list is offering - the command itself - and the second
      // sends it.
      if (send) {
        for (let press = 0; press < 2; press += 1) {
          field.dispatchEvent(
            new KeyboardEvent('keydown', { key: 'Enter', bubbles: true, cancelable: true }),
          );
        }
      }
    }
    if (click !== null) {
      // The state the props put up has drawn by now; the press is the reader's.
      await tick();
      box?.querySelector<HTMLElement>(click)?.click();
    }
  });
</script>

<div class="comp" bind:this={box}>
  <Composer {...props} />
</div>
