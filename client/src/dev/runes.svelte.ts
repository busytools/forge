/**
 * A `.svelte.ts` module: reactive state that is not a component, which is a
 * shape `eslint.config.js` has to carry for the client. Nothing imports it -
 * it exists so that config block is exercised, and dropping it fails the gate
 * here rather than in whatever module next needs one.
 */
export const probe = $state(0);
