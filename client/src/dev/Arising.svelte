<script lang="ts">
  /**
   * A composer whose body an ask takes while the reader's draft is already
   * written: the state that puts the draft chip on the live row.
   *
   * **No prop reaches it.** The draft is the reader's own, held by the
   * composer for the seat, and an ask arriving takes the body - so the page
   * types the words first and THEN hands the composer the ask, which is the
   * two events a person produces rather than a copy of the result.
   *
   * Test support for the by-width page: nothing the app ships imports it.
   */
  import { onMount } from 'svelte';

  import Composer from '../composer/Composer.svelte';
  import type { ComposerProps } from '../composer/view';

  let { initial, draft, later }: { initial: ComposerProps; draft: string; later: ComposerProps } =
    $props();
  // The initial value is the point: this is the page standing still before the
  // reader types, and the ask that arrives after.
  // svelte-ignore state_referenced_locally
  let page = $state<ComposerProps>(initial);
  let box = $state<HTMLDivElement | null>(null);

  onMount(() => {
    const field = box?.querySelector('textarea');
    if (field instanceof HTMLTextAreaElement) {
      field.value = draft;
      field.dispatchEvent(new Event('input', { bubbles: true }));
    }
    page = later;
  });
</script>

<div class="comp" bind:this={box}>
  <Composer {...page} />
</div>
