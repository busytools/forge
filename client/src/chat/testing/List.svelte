<script lang="ts">
  import type { Snippet } from 'svelte';

  import { element, list, pins, records, register, reported } from './records';

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
    return element.offset;
  }

  export function getScrollSize(): number {
    return reported.size;
  }

  export function getViewportSize(): number {
    return reported.viewport;
  }

  /**
   * Whole pixels, as a browser reports both of the element's sizes.
   *
   * Floored here, which agrees with both engines on whole values; their
   * reported sizes round to NEAREST on fractional ones, so `Math.round` is
   * the faithful pair - nothing rests on the difference today.
   */
  const elementHeight = (): number => Math.floor(element.height);
  const elementViewport = (): number => Math.floor(element.viewport);

  /**
   * The element the column scrolls, made to behave like a scroll container.
   *
   * **jsdom performs no layout, so this is the layout the column's pin needs.**
   * The element reports the size the test gave it and clamps what it is asked
   * for, exactly as a browser clamps a scroll: the column pins the foot by
   * asking for `scrollHeight`, and what it LANDS on is the foot.
   */
  function container(node: HTMLElement): () => void {
    Object.defineProperty(node, 'scrollHeight', {
      configurable: true,
      get: elementHeight,
    });
    Object.defineProperty(node, 'clientHeight', {
      configurable: true,
      get: elementViewport,
    });
    Object.defineProperty(node, 'scrollTop', {
      configurable: true,
      get: () => element.offset,
      set: (asked: number) => {
        const landed = Math.max(0, Math.min(asked, elementHeight() - elementViewport()));
        pins.push({ asked, landed });
        element.offset = landed;
      },
    });
    return () => undefined;
  }

  /** Where the list is scrolled to, which a test drives the reader with. */
  export function scrolledTo(at: number, total: number, height: number): void {
    element.offset = at;
    reported.size = total;
    reported.viewport = height;
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
    element.offset = at;
    reported.size = total;
    reported.viewport = height;
  }

  // The column binds this component to a handle of its own, which a test
  // cannot reach; the module is the seam instead.
  const mine = { scrolledTo, settled };

  // **And the handle lives exactly as long as the effect does.** A real list
  // removes its scroll listener when it is destroyed, in the same flush that
  // removes the node, so nothing may drive a column through a list that is
  // gone - a seam left registered would let a test call a callback the browser
  // cannot deliver. Registering inside the effect pairs it with that teardown:
  // a list unmounted before its first flush never registers at all. Cleared
  // only while the seam still holds THIS list, because Svelte creates the
  // incoming branch before running the outgoing branch's destroy.
  $effect(() => {
    register(mine);
    return () => {
      if (list() === mine) register(null);
    };
  });
</script>

<div class="conv" {@attach container} {...rest}>
  {#each data as row, at (keyOf(row, at))}
    <div class="turn">{@render children?.(row)}</div>
  {/each}
</div>
