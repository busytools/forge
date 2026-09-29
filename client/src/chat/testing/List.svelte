<script lang="ts">
  import type { Snippet } from 'svelte';

  import { records, register } from './records';

  /**
   * A stand-in for `virtua`'s list, for tests that have to see what the column
   * HANDS it rather than what it draws.
   *
   * **The compensation is read once, as the row count changes.** `virtua`
   * reads `shift` from an effect that runs before the DOM update and consults
   * it nowhere else, which is why the timing of arming it is the whole defect
   * and why nothing in a browser can pin it: the difference between armed and
   * not is one frame, and one frame of a jump is not something a screenshot
   * holds.
   */
  let {
    data = [],
    shift = false,
    onscroll,
    children,
  }: {
    data?: unknown[];
    shift?: boolean;
    onscroll?: (offset: number) => void;
    children?: Snippet<[unknown]>;
  } = $props();

  let offset = 0;
  let size = 0;
  let viewport = 0;

  // Where virtua reads it: before the DOM updates, on the change itself.
  $effect.pre(() => {
    records.push({ length: data.length, shift });
  });

  export function getScrollOffset(): number {
    return offset;
  }

  export function getScrollSize(): number {
    return size;
  }

  export function getViewportSize(): number {
    return viewport;
  }

  export function scrollToIndex(): void {
    return undefined;
  }

  /** Where the list is scrolled to, which a test drives the reader with. */
  export function scrolledTo(at: number, total: number, height: number): void {
    offset = at;
    size = total;
    viewport = height;
    onscroll?.(at);
  }

  // The column binds this component to a handle of its own, which a test
  // cannot reach; the module is the seam instead.
  register({ scrolledTo });
</script>

<div class="conv">
  {#each data as row, at (at)}
    <div class="turn">{@render children?.(row)}</div>
  {/each}
</div>
