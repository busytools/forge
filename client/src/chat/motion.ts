/**
 * The one motion a message makes, from the pile's card to the chat's row.
 *
 * **The card and the row are the same item in two views**, so acceptance moves
 * it rather than popping one out and another in: the card's rect is remembered
 * as it leaves, and the row it becomes lands from there - a FLIP - so the eye
 * follows one thing that moved rather than two things that changed.
 *
 * **Reduced motion keeps the position change and drops the animation.** The
 * row still appears where it belongs; it simply arrives there instead of
 * travelling. That is `wantsMotion`'s rule, shared with the scroll glide, so
 * one preference means one thing across the page.
 */

import { wantsMotion } from './glide';

/** How long a landing takes, in milliseconds. */
export const LAND_MS = 240;

/** How long an arrival's rise takes, in milliseconds. */
export const RISE_MS = 180;

/** How long a departure stays claimable, so a stale rect never lands a stranger. */
const FLIGHT_MS = 2_000;

/** Where one item left the pile, in viewport coordinates. */
export interface Flight {
  left: number;
  top: number;
  width: number;
  height: number;
}

interface Claim {
  from: Flight;
  at: number;
}

const flights = new Map<string, Claim>();

/**
 * Where `uuid`'s card was drawn on the pile's last pass.
 *
 * **Measured while the card is still there**, not when it leaves. The row and
 * the card are updated by two stores in the same paint, so a departure
 * measured on the way out can run after the row has already mounted; a
 * position recorded on the pass before is there whichever order the two run
 * in. Bounded, because a row that never lands - a prompt cancelled on the way
 * - must not keep its rect for the session.
 */
export function drawnAt(
  uuid: string,
  rect: Pick<Flight, 'left' | 'top' | 'width' | 'height'>,
): void {
  flights.delete(uuid);
  flights.set(uuid, { from: rect, at: performance.now() });
  while (flights.size > 32) {
    const oldest = flights.keys().next().value;
    if (oldest === undefined) break;
    flights.delete(oldest);
  }
}

/**
 * Claim the landing for `uuid`, if the card's last position is still fresh.
 *
 * **One claim per position**: a second copy of the row - a virtualised list
 * remounting it, a second column on the same seat - is not the item that left,
 * and animating it would draw the move twice.
 */
export function lastDrawn(uuid: string): Flight | null {
  const claim = flights.get(uuid);
  if (claim === undefined) return null;
  flights.delete(uuid);
  if (performance.now() - claim.at > FLIGHT_MS) return null;
  return claim.from;
}

const arrived = new Set<string>();

/** A user row was appended live, so its first mount may rise into place. */
export function noteArrival(uuid: string): void {
  arrived.add(uuid);
  // One beat is the whole window: a row that mounts later than this mounted
  // because the reader scrolled to it, not because it arrived.
  setTimeout(() => arrived.delete(uuid), RISE_MS * 4);
}

/** Whether this row's mount is an arrival, which it may claim once. */
export function arrival(uuid: string): boolean {
  return arrived.delete(uuid);
}

/**
 * Land `node` from `from`, over the page's one motion.
 *
 * Answers the cancel, which restores the node's own style untouched.
 */
export function land(node: HTMLElement, from: Flight): () => void {
  const done = (): void => {
    node.style.transition = '';
    node.style.transform = '';
    node.style.transformOrigin = '';
    node.style.opacity = '';
  };
  if (!wantsMotion()) return done;
  const to = node.getBoundingClientRect();
  if (to.width === 0 || to.height === 0) return done;
  const dx = from.left - to.left;
  const dy = from.top - to.top;
  const sx = from.width / to.width;
  const sy = from.height / to.height;
  node.style.transformOrigin = 'top left';
  node.style.transition = 'none';
  node.style.transform = `translate(${dx}px, ${dy}px) scale(${sx}, ${sy})`;
  node.style.opacity = '0.55';
  // The write above has to have landed before the transition can run from it.
  void node.offsetWidth;
  node.style.transition = `transform ${LAND_MS}ms cubic-bezier(0.2, 0.7, 0.2, 1), opacity ${LAND_MS}ms linear`;
  node.style.transform = 'translate(0, 0) scale(1, 1)';
  node.style.opacity = '1';
  const timer = setTimeout(done, LAND_MS + 40);
  return () => {
    clearTimeout(timer);
    done();
  };
}

/**
 * Fade `node` in on mount, for a card whose transform is already the pile's
 * own: opacity is the one property the depth arithmetic does not write.
 */
export function appear(node: HTMLElement): { destroy: () => void } {
  const done = (): void => {
    node.style.transition = '';
    node.style.opacity = '';
  };
  if (!wantsMotion()) return { destroy: done };
  node.style.transition = 'none';
  node.style.opacity = '0';
  void node.offsetWidth;
  node.style.transition = `opacity ${RISE_MS}ms ease-out`;
  node.style.opacity = '1';
  const timer = setTimeout(done, RISE_MS + 40);
  return {
    destroy: () => {
      clearTimeout(timer);
      done();
    },
  };
}

/** Rise `node` into place, over the same preference. */
export function rise(node: HTMLElement): () => void {
  const done = (): void => {
    node.style.transition = '';
    node.style.transform = '';
    node.style.opacity = '';
  };
  if (!wantsMotion()) return done;
  node.style.transition = 'none';
  node.style.transform = 'translateY(6px)';
  node.style.opacity = '0';
  void node.offsetWidth;
  node.style.transition = `transform ${RISE_MS}ms ease-out, opacity ${RISE_MS}ms ease-out`;
  node.style.transform = 'translateY(0)';
  node.style.opacity = '1';
  const timer = setTimeout(done, RISE_MS + 40);
  return () => {
    clearTimeout(timer);
    done();
  };
}
