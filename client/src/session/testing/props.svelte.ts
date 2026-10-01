/**
 * Props a test can move after mounting, which a plain object is not.
 *
 * Svelte 5 reads the `props` object reactively, so a component mounted with
 * one of these follows a write to it - which is what lets a test reproduce the
 * thing a redraw does: a new occupant under a cell that is already on screen.
 * Runes compile only in `.svelte` and `.svelte.ts` files, so it lives here.
 */
export function boxed<T extends Record<string, unknown>>(initial: T): T {
  // Declared and returned rather than returned in one expression: a rune is
  // only allowed as a declaration's initializer.
  const held = $state(initial);
  return held;
}
