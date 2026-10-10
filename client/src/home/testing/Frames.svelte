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
   * Every frame restamps the build the header draws, so a test can prove the
   * frame was DELIVERED - a count of the interval's own fires cannot tell a
   * page that received the wire from one that never re-read it.
   */
  let { initial, everyMs }: { initial: HomeWire; everyMs: number } = $props();

  // svelte-ignore state_referenced_locally
  let wire = $state(initial);
  let frames = 0;

  onMount(() => {
    const id = setInterval(() => {
      frames += 1;
      wire = { ...wire, forge_version_short: `frame-${frames}` };
    }, everyMs);
    return () => clearInterval(id);
  });
</script>

<Home {wire} />
