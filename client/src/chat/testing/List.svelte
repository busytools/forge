<script lang="ts">
  import type { Snippet } from 'svelte';

  import { element, geometry, pins, records, register } from './records';

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
    getKey,
    onscroll,
    children,
    ...rest
  }: {
    data?: unknown[];
    shift?: boolean;
    /**
     * How the real list identifies a row, which THIS STUB HAS TO HONOUR.
     *
     * Keyed by position, the stub reuses a row's DOM through a change of
     * identity - so a test asserting what a row keeps across an update passes
     * here and fails in a browser, which is exactly what a blind pin looks
     * like.
     */
    getKey?: (row: unknown) => string;
    onscroll?: (offset: number) => void;
    children?: Snippet<[unknown]>;
  } = $props();

  /** What a row is keyed by: the list's own keyer, or its position without one. */
  function keyOf(row: unknown, at: number): string {
    return getKey === undefined ? `${at}` : getKey(row);
  }

  // Where virtua reads it: before the DOM updates, on the change itself.
  $effect.pre(() => {
    records.push({ length: data.length, shift });
  });

  export function getScrollOffset(): number {
    return geometry.offset;
  }

  export function getScrollSize(): number {
    return geometry.size;
  }

  export function getViewportSize(): number {
    return geometry.viewport;
  }

  /**
   * The element the column scrolls, made to behave like a scroll container.
   *
   * **jsdom performs no layout, so this is the layout the column's pin needs.**
   * The element reports the size the test gave it and clamps what it is asked
   * for, exactly as a browser clamps a scroll: the column pins the foot by
   * asking for `scrollHeight`, and what it LANDS on is the foot.
   */
  function container(node: HTMLElement): () => void {
    // Whole pixels, as a browser reports them, from a size the list measured
    // as a fraction - which is the mismatch the follow has to survive.
    Object.defineProperty(node, 'scrollHeight', {
      configurable: true,
      get: () => Math.floor(element.height),
    });
    // The element's own viewport, which the list's model can lag behind: a
    // column that reads the model for this reads zero until it has measured.
    Object.defineProperty(node, 'clientHeight', {
      configurable: true,
      get: () => element.viewport,
    });
    Object.defineProperty(node, 'scrollTop', {
      configurable: true,
      get: () => geometry.offset,
      set: (asked: number) => {
        const landed = Math.max(0, Math.min(asked, Math.floor(element.height) - element.viewport));
        pins.push({ asked, landed });
        geometry.offset = landed;
      },
    });
    return () => undefined;
  }

  /** Where the list is scrolled to, which a test drives the reader with. */
  export function scrolledTo(at: number, total: number, height: number): void {
    geometry.offset = at;
    geometry.size = total;
    geometry.viewport = height;
    onscroll?.(at);
  }

  /**
   * A clamp the browser made with nobody scrolling.
   *
   * A window grown until the history fits leaves the reader at the very end
   * and fires no scroll event to say so, which is the case the follow's own
   * clamp re-arm exists for.
   */
  export function settled(at: number, total: number, height: number): void {
    geometry.offset = at;
    geometry.size = total;
    geometry.viewport = height;
  }

  // The column binds this component to a handle of its own, which a test
  // cannot reach; the module is the seam instead.
  register({ scrolledTo, settled });
</script>

<div class="conv" {@attach container} {...rest}>
  {#each data as row, at (keyOf(row, at))}
    <div class="turn">{@render children?.(row)}</div>
  {/each}
</div>
