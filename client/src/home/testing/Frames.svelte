<script lang="ts">
  import { onMount } from 'svelte';

  import type { HomeWire } from '../../wire/home';
  import Home from '../Home.svelte';

  /**
   * The home with frames that keep landing.
   *
   * A frame is a fresh snapshot, and the ages read a clock armed in an effect
   * - so a frame landing faster than the clock's thirty seconds is exactly
   * the case where a timer that re-arms per frame never fires and the ages
   * freeze while the fleet keeps moving. The swap is a fresh object every
   * `everyMs`, which is what a changed read looks like to the effect.
   *
   * The count is drawn for the test: an assertion that the ages moved would
   * otherwise pass with no frame landing at all.
   */
  let { initial, everyMs }: { initial: HomeWire; everyMs: number } = $props();

  // svelte-ignore state_referenced_locally
  let wire = $state(initial);
  let frames = $state(0);

  onMount(() => {
    const id = setInterval(() => {
      frames += 1;
      wire = { ...wire };
    }, everyMs);
    return () => clearInterval(id);
  });
</script>

<Home {wire} />
<span class="frames-count" style="display: none">{frames}</span>
