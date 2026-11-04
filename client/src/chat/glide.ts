/**
 * The short glide a following column moves its viewport with.
 *
 * **A following column holds the moving foot**, and a foot that moves in one
 * write per frame arrives as a jump with the motion made by the content
 * instead of the viewport. The glide is the missing half: every landing aims
 * at the measured foot and the viewport travels there over a short ease, so a
 * streamed line, a row measuring taller and a turn landing all read as the
 * column moving rather than the page snapping.
 *
 * **A reader who asked for less motion gets the landing itself.** The duration
 * is the only thing the preference changes; the destination, the read-back and
 * every decision around the landing are the same.
 */

/** How long a glide takes, in milliseconds. */
export const GLIDE_MS = 160;

/**
 * Whether this page wants the glide.
 *
 * **A page with no media-query support does not animate.** Every browser has
 * `matchMedia`; what lacks it is a test or a headless renderer, where a frame
 * loop would be a wait with nothing painting. The safe answer is the landing.
 */
export function wantsMotion(): boolean {
  return (
    typeof window !== 'undefined' &&
    typeof window.matchMedia === 'function' &&
    !window.matchMedia('(prefers-reduced-motion: reduce)').matches
  );
}

/** The part of a scrollable element the glide writes to. */
export interface Scroller {
  scrollTop: number;
}

/**
 * Move `box` to `target` over the glide's ease, answering what cancels it.
 *
 * **`done` runs on the frame the target is written**, after the write, so the
 * caller reads back what the browser clamped that write to. A cancel drops
 * the frames still queued and calls nothing.
 */
export function glideTo(box: Scroller, target: number, done?: () => void): () => void {
  const from = box.scrollTop;
  const start = performance.now();
  let frame = 0;
  const step = (): void => {
    const through = Math.min(1, (performance.now() - start) / GLIDE_MS);
    // Cubic ease-out: the viewport leaves quickly and settles onto the foot,
    // which reads as following rather than as a scroll with a start and a stop.
    box.scrollTop = from + (target - from) * (1 - (1 - through) ** 3);
    if (through < 1) {
      frame = requestAnimationFrame(step);
      return;
    }
    done?.();
  };
  frame = requestAnimationFrame(step);
  return () => cancelAnimationFrame(frame);
}
